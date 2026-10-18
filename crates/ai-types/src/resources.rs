//! Explicit client-side limits. A shared inference server remains responsible
//! for its own process/GPU allocation; these limits are not an OS sandbox.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LocalResourcePolicy {
    pub cpu_threads: u16,
    pub context_tokens: u32,
    pub batch_tokens: u16,
    pub use_gpu: bool,
    pub min_free_ram_mb: u32,
    pub max_memory_percent: u8,
    pub max_image_edge: u32,
    pub keep_alive_seconds: u16,
}

impl Default for LocalResourcePolicy {
    fn default() -> Self {
        Self {
            cpu_threads: 4,
            context_tokens: 16_384,
            batch_tokens: 128,
            use_gpu: true,
            min_free_ram_mb: 2048,
            max_memory_percent: 85,
            max_image_edge: 2048,
            keep_alive_seconds: 0,
        }
    }
}

impl LocalResourcePolicy {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=64).contains(&self.cpu_threads)
            || !(2048..=65_536).contains(&self.context_tokens)
            || !(16..=512).contains(&self.batch_tokens)
            || !(512..=131_072).contains(&self.min_free_ram_mb)
            || !(50..=95).contains(&self.max_memory_percent)
            || !(256..=4096).contains(&self.max_image_edge)
            || self.keep_alive_seconds > 300
        {
            return Err("Local resource settings exceed the supported safe ranges".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GpuSnapshot {
    pub name: String,
    pub total_vram_mb: u64,
    pub free_vram_mb: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub cpu_threads_available: u32,
    pub total_ram_mb: u64,
    pub available_ram_mb: u64,
    pub gpus: Vec<GpuSnapshot>,
    pub observed_at_unix_ms: u64,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_bounded_and_unknown_controls_rejected() {
        assert!(LocalResourcePolicy::default().validate().is_ok());
        assert!(serde_json::from_str::<LocalResourcePolicy>(r#"{"shell":"run"}"#).is_err());
        for percent in [0, 49, 96, 255] {
            assert!(LocalResourcePolicy { max_memory_percent: percent, ..Default::default() }.validate().is_err());
        }
        assert!(LocalResourcePolicy { cpu_threads: 0, ..Default::default() }.validate().is_err());
        assert!(LocalResourcePolicy { max_image_edge: u32::MAX, ..Default::default() }.validate().is_err());
    }
}
