//! Admission limits and measured usage. Monetary amounts are rate-based estimates.
use designcraft_ai_providers::Usage;
use designcraft_ai_types::ProviderKind;
use designcraft_ai_types::protocol::{BudgetSpec, TokenRates, UsageSummary};

#[derive(Clone, Debug)]
pub struct Reservation {
    input: u64,
    output: u64,
    cost: Option<f64>,
    rates: Option<TokenRates>,
    local: bool,
    image: bool,
}
#[derive(Clone, Debug)]
pub struct UsageLedger {
    budget: BudgetSpec,
    summary: UsageSummary,
    charged_tokens: u64,
    charged_cost: f64,
}

impl UsageLedger {
    pub fn new(budget: BudgetSpec) -> Result<Self, String> {
        if !(256..=4_000_000).contains(&budget.token_limit)
            || budget.max_output_tokens == 0
            || budget.max_output_tokens > 65_536
            || budget.max_requests == 0
            || budget.max_requests > 64
            || budget.repair_attempts > 3
        {
            return Err("Invalid token, request or repair limits".into());
        }
        if budget.max_cost_usd.is_some_and(|n| !n.is_finite() || n <= 0.0) {
            return Err("The monetary limit must be finite and positive".into());
        }
        if let Some(rates) = &budget.rates {
            validate_rates(rates)?;
        }
        Ok(Self { budget, summary: UsageSummary::default(), charged_tokens: 0, charged_cost: 0.0 })
    }

    /// Reserve before every external call. Text bytes conservatively approximate
    /// input tokens; image allowances are estimates and are labelled as such.
    pub fn reserve(
        &mut self,
        input_bytes: usize,
        images: usize,
        provider: ProviderKind,
        image_call: bool,
        image_rates: Option<&TokenRates>,
    ) -> Result<Reservation, String> {
        if self.summary.requests >= self.budget.max_requests {
            return Err("AI request limit reached".into());
        }
        let input = u64::try_from(input_bytes)
            .map_err(|_| "AI input is too large")?
            .saturating_add(1024)
            .saturating_add((images as u64).saturating_mul(8192));
        let output = if image_call { 0 } else { u64::from(self.budget.max_output_tokens) };
        let allowance = input.saturating_add(output);
        if self.charged_tokens.saturating_add(allowance) > self.budget.token_limit {
            return Err("Token budget cannot admit another request; shorten context or raise the limit".into());
        }
        let local = matches!(provider, ProviderKind::Ollama | ProviderKind::ComfyUi);
        let rates = if image_call { image_rates.cloned() } else { self.budget.rates.clone() };
        if let Some(r) = &rates {
            validate_rates(r)?;
        }
        let cost = if local {
            Some(0.0)
        } else {
            rates.as_ref().and_then(|r| {
                if image_call {
                    r.image_per_request.or(r.image_reserve_usd)
                } else {
                    Some(
                        (input as f64 * r.input_per_million.max(r.cache_write_per_million.unwrap_or(r.input_per_million))
                            + output as f64 * r.output_per_million)
                            / 1_000_000.0,
                    )
                }
            })
        };
        if let Some(limit) = self.budget.max_cost_usd {
            let estimate = cost.ok_or("Set rates for the selected provider before using a monetary limit")?;
            if self.charged_cost + estimate > limit {
                return Err("Estimated monetary budget cannot admit another request".into());
            }
        }
        self.summary.requests = self.summary.requests.saturating_add(1);
        self.charged_tokens = self.charged_tokens.saturating_add(allowance);
        self.charged_cost += cost.unwrap_or(0.0);
        Ok(Reservation { input, output, cost, rates, local, image: image_call })
    }

    pub fn record(&mut self, reservation: Reservation, usage: Option<&Usage>) {
        let measured = usage.and_then(|u| Some((u.input_tokens?, u.output_tokens?)));
        let missing = usage.is_none() || measured.is_none();
        self.summary.has_unreported_usage |= missing;
        let input = usage.and_then(|u| u.input_tokens).unwrap_or(0);
        let output = usage.and_then(|u| u.output_tokens).unwrap_or(0);
        let cached = usage.and_then(|u| u.cached_input_tokens).unwrap_or(0).min(input);
        let cache_write = usage.and_then(|u| u.cache_write_tokens).unwrap_or(0).min(input.saturating_sub(cached));
        let reasoning = usage.and_then(|u| u.reasoning_tokens).unwrap_or(0);
        self.summary.input_tokens = self.summary.input_tokens.saturating_add(input);
        self.summary.output_tokens = self.summary.output_tokens.saturating_add(output);
        self.summary.cached_input_tokens = self.summary.cached_input_tokens.saturating_add(cached);
        self.summary.reasoning_tokens = self.summary.reasoning_tokens.saturating_add(reasoning);
        let estimated = reservation.input.saturating_add(reservation.output);
        if missing {
            self.summary.estimated_tokens = self.summary.estimated_tokens.saturating_add(estimated);
        }
        if let Some((i, o)) = measured {
            self.charged_tokens = self.charged_tokens.saturating_sub(estimated).saturating_add(i.saturating_add(o));
        }
        let cost = if reservation.local {
            Some(0.0)
        } else if reservation.image && reservation.rates.as_ref().is_some_and(|r| r.image_per_request.is_some()) {
            reservation.cost
        } else if let (Some((i, o)), Some(r)) = (measured, &reservation.rates) {
            Some(
                ((i.saturating_sub(cached).saturating_sub(cache_write)) as f64 * r.input_per_million
                    + cached as f64 * r.cached_input_per_million
                    + cache_write as f64 * r.cache_write_per_million.unwrap_or(r.input_per_million)
                    + o as f64 * r.output_per_million)
                    / 1_000_000.0,
            )
        } else {
            reservation.cost
        };
        if let Some(old) = reservation.cost {
            self.charged_cost -= old;
        }
        self.charged_cost += cost.unwrap_or(0.0);
        if self.summary.requests == 1 {
            self.summary.estimated_cost_usd = cost;
        } else {
            self.summary.estimated_cost_usd = self.summary.estimated_cost_usd.zip(cost).map(|(a, b)| a + b);
        }
    }
    pub fn summary(&self) -> UsageSummary {
        self.summary.clone()
    }

    /// Reporting only; admission always retains this full ledger across batch workers.
    pub fn delta_since(&self, before: &UsageSummary) -> UsageSummary {
        let current = &self.summary;
        UsageSummary {
            requests: current.requests.saturating_sub(before.requests),
            input_tokens: current.input_tokens.saturating_sub(before.input_tokens),
            output_tokens: current.output_tokens.saturating_sub(before.output_tokens),
            cached_input_tokens: current.cached_input_tokens.saturating_sub(before.cached_input_tokens),
            reasoning_tokens: current.reasoning_tokens.saturating_sub(before.reasoning_tokens),
            estimated_tokens: current.estimated_tokens.saturating_sub(before.estimated_tokens),
            estimated_cost_usd: if before.requests == 0 {
                current.estimated_cost_usd
            } else {
                current.estimated_cost_usd.zip(before.estimated_cost_usd).map(|(now, old)| (now - old).max(0.0))
            },
            has_unreported_usage: current.estimated_tokens > before.estimated_tokens,
        }
    }
}

pub fn validate_rates(r: &TokenRates) -> Result<(), String> {
    if [
        Some(r.input_per_million),
        Some(r.cached_input_per_million),
        Some(r.output_per_million),
        r.cache_write_per_million,
        r.image_per_request,
        r.image_reserve_usd,
    ]
    .into_iter()
    .flatten()
    .any(|n| !n.is_finite() || !(0.0..=1_000_000.0).contains(&n))
    {
        return Err("Provider rates must be finite, nonnegative values".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transferred_batch_ledger_retains_unknown_charges_and_reports_only_delta() {
        let mut ledger = UsageLedger::new(BudgetSpec { token_limit: 10_000, max_requests: 3, ..Default::default() }).unwrap();
        let before = ledger.summary();
        let reservation = ledger.reserve(100, 0, ProviderKind::Ollama, false, None).unwrap();
        ledger.record(reservation, None);
        let delta = ledger.delta_since(&before);
        assert_eq!(delta.requests, 1);
        assert!(delta.has_unreported_usage);
        assert!(delta.estimated_tokens > 9000);
        fn following_batch(mut ledger: UsageLedger) -> UsageLedger {
            assert!(ledger.reserve(1, 0, ProviderKind::Ollama, false, None).is_err(), "Unknown first-batch usage must keep its admission charge");
            ledger
        }
        let snapshot = ledger.summary();
        let ledger = following_batch(ledger);
        assert_eq!(ledger.delta_since(&snapshot).requests, 0);
        assert_eq!(ledger.summary().requests, 1);
    }
    #[test]
    fn requests_reserve_before_sending_and_missing_usage_is_not_free() {
        let mut ledger = UsageLedger::new(BudgetSpec { token_limit: 20_000, max_requests: 1, ..Default::default() }).unwrap();
        let reservation = ledger.reserve(100, 0, ProviderKind::Ollama, false, None).unwrap();
        ledger.record(reservation, None);
        assert!(ledger.reserve(1, 0, ProviderKind::Ollama, false, None).is_err());
        assert!(ledger.summary().has_unreported_usage);
        assert!(ledger.summary().estimated_tokens > 8192);
    }
    #[test]
    fn paid_budget_needs_rates_and_counts_measured_cached_usage() {
        let mut budget = BudgetSpec { max_cost_usd: Some(1.0), ..Default::default() };
        assert!(UsageLedger::new(budget.clone()).unwrap().reserve(10, 0, ProviderKind::OpenAi, false, None).is_err());
        budget.rates = Some(TokenRates {
            input_per_million: 2.0,
            cached_input_per_million: 0.2,
            output_per_million: 8.0,
            cache_write_per_million: None,
            image_per_request: None,
            image_reserve_usd: None,
        });
        let mut ledger = UsageLedger::new(budget).unwrap();
        let reservation = ledger.reserve(200, 0, ProviderKind::OpenAi, false, None).unwrap();
        ledger.record(
            reservation,
            Some(&Usage {
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cached_input_tokens: Some(500),
                reasoning_tokens: Some(50),
                ..Default::default()
            }),
        );
        let s = ledger.summary();
        assert_eq!(s.cached_input_tokens, 500);
        assert!((s.estimated_cost_usd.unwrap() - 0.0027).abs() < 1e-10);
        assert!(!s.has_unreported_usage);
    }
    #[test]
    fn rejects_nan_and_oversized_requests() {
        assert!(UsageLedger::new(BudgetSpec { max_cost_usd: Some(f64::NAN), ..Default::default() }).is_err());
        let mut ledger = UsageLedger::new(BudgetSpec::default()).unwrap();
        assert!(ledger.reserve(100_000, 0, ProviderKind::Ollama, false, None).is_err());
        assert_eq!(ledger.summary().requests, 0);
    }
    #[test]
    fn token_priced_images_reserve_before_sending_then_use_measured_cost() {
        let mut ledger = UsageLedger::new(BudgetSpec { max_cost_usd: Some(0.5), ..Default::default() }).unwrap();
        let mut rates = TokenRates {
            input_per_million: 2.0,
            cached_input_per_million: 0.2,
            output_per_million: 8.0,
            cache_write_per_million: None,
            image_per_request: None,
            image_reserve_usd: Some(0.2),
        };
        let reservation = ledger.reserve(100, 1, ProviderKind::OpenAi, true, Some(&rates)).unwrap();
        ledger.record(reservation, Some(&Usage { input_tokens: Some(1000), output_tokens: Some(2000), ..Default::default() }));
        assert!((ledger.summary().estimated_cost_usd.unwrap() - 0.018).abs() < 1e-10);
        rates.image_per_request = Some(0.1);
        let reservation = ledger.reserve(100, 1, ProviderKind::OpenAi, true, Some(&rates)).unwrap();
        ledger.record(reservation, Some(&Usage { input_tokens: Some(1000), output_tokens: Some(2000), ..Default::default() }));
        assert!((ledger.summary().estimated_cost_usd.unwrap() - 0.118).abs() < 1e-10);
        rates.image_per_request = None;
        rates.image_reserve_usd = Some(0.49);
        assert!(ledger.reserve(100, 1, ProviderKind::OpenAi, true, Some(&rates)).is_err());
    }
}
