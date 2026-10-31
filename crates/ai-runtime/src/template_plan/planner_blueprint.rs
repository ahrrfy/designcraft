//! Deterministic master blueprint synthesizer for planners, journals, notebooks,
//! 365-page year-round stationery editions, and commercial print collateral.
use super::curriculum::{extract_prompt_topic, normalize_curriculum_query};
use super::{profile, scope_end};
use crate::worker::BatchScope;
use designcraft_ai_skills::SkillSnapshot;
use designcraft_ai_types::design::DesignProfile;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(crate) struct SeasonTheme {
    pub month: u8,
    pub name_en: &'static str,
    pub name_ar: &'static str,
    pub mood_en: &'static str,
    pub mood_ar: &'static str,
    pub palette_cue: &'static str,
    pub botanical_motif: &'static str,
}

pub(crate) const SEASONS: [SeasonTheme; 12] = [
    SeasonTheme {
        month: 1,
        name_en: "January",
        name_ar: "يناير",
        mood_en: "Frost & First Starlight",
        mood_ar: "نجمة البدايات والصفاء الشتوي",
        palette_cue: "Soft sage, ice mist & warm cream",
        botanical_motif: "winter eucalyptus sprigs, snowdrop buds and subtle starlight dust",
    },
    SeasonTheme {
        month: 2,
        name_en: "February",
        name_ar: "فبراير",
        mood_en: "Almond Blossom & Kindness",
        mood_ar: "زهر اللوز والمودة",
        palette_cue: "Dusty rose, petal blush & soft sage",
        botanical_motif: "delicate pale pink almond blossoms on slender twigs",
    },
    SeasonTheme {
        month: 3,
        name_en: "March",
        name_ar: "مارس",
        mood_en: "Vernal Garden Awakening",
        mood_ar: "إشراقة الربيع وتجدد الأثر",
        palette_cue: "Fresh mint leaf, botanical sage & cream",
        botanical_motif: "sprouting spring fern fronds, chamomile buds and dewdrops",
    },
    SeasonTheme {
        month: 4,
        name_en: "April",
        name_ar: "أبريل",
        mood_en: "Dewdrop Meadow & Clarity",
        mood_ar: "ندى المروج وصفاء الذهن",
        palette_cue: "Lavender mist, sky wash & sage",
        botanical_motif: "soft lavender stems, sweet pea petals and airy watercolor clouds",
    },
    SeasonTheme {
        month: 5,
        name_en: "May",
        name_ar: "مايو",
        mood_en: "Wildflower Sunbeams",
        mood_ar: "أزهار الحقول وطاقة الإنجاز",
        palette_cue: "Warm buttercup, peach wash & olive leaf",
        botanical_motif: "pressed meadow wildflowers, daisies and golden pollen accents",
    },
    SeasonTheme {
        month: 6,
        name_en: "June",
        name_ar: "يونيو",
        mood_en: "Citrus Orchard & Momentum",
        mood_ar: "بستان الصيف وانطلاقة الشغف",
        palette_cue: "Soft apricot, neroli cream & botanical green",
        botanical_motif: "orange blossom sprigs, miniature citrus leaves and sunlit washes",
    },
    SeasonTheme {
        month: 7,
        name_en: "July",
        name_ar: "يوليو",
        mood_en: "Coastal Breeze & Serenity",
        mood_ar: "نسيم البحر والهدوء العميق",
        palette_cue: "Seafoam wash, mist blue & warm sand",
        botanical_motif: "sea-lavender sprigs, olive branches and calm coastal watercolor mist",
    },
    SeasonTheme {
        month: 8,
        name_en: "August",
        name_ar: "أغسطس",
        mood_en: "Golden Harvest & Focus",
        mood_ar: "سنابل الذهب والتركيز المثمر",
        palette_cue: "Honey ochre, warm sage & parchment",
        botanical_motif: "golden wheat stalks, wild rosemary and sunlit botanical leaves",
    },
    SeasonTheme {
        month: 9,
        name_en: "September",
        name_ar: "سبتمبر",
        mood_en: "Botanical Library & Study",
        mood_ar: "مكتبة الخريف وحب التعلم",
        palette_cue: "Eucalyptus green, warm taupe & cream",
        botanical_motif: "pressed ginkgo leaves, sage branches and vintage herbarium twigs",
    },
    SeasonTheme {
        month: 10,
        name_en: "October",
        name_ar: "أكتوبر",
        mood_en: "Cinnamon Leaf & Cozy Craft",
        mood_ar: "أوراق الخريف ودفء الإبداع",
        palette_cue: "Soft terracotta, amber wash & muted sage",
        botanical_motif: "autumn maple leaves, rosehip berries and warm cinnamon botanical washes",
    },
    SeasonTheme {
        month: 11,
        name_en: "November",
        name_ar: "نوفمبر",
        mood_en: "Starlit Gratitude & Calm",
        mood_ar: "سماء الامتنان والسكينة",
        palette_cue: "Twilight mauve, dusty plum & warm cream",
        botanical_motif: "dried fig leaves, silver dollar eucalyptus and gentle crescent starlight",
    },
    SeasonTheme {
        month: 12,
        name_en: "December",
        name_ar: "ديسمبر",
        mood_en: "Evergreen Reflection & Legacy",
        mood_ar: "حصاد العام وأثر البهجة",
        palette_cue: "Pine sage, warm gold ochre & ivory",
        botanical_motif: "cedar needles, winter berries, white hellebore blossoms and starlight",
    },
];

const MICRO_JOYS: [(&str, &str); 24] = [
    ("Five slow morning breaths beside an open window", "خمس أنفاس صباحية هادئة قرب النافذة"),
    ("Write one sincere thank-you message today", "رسالة شكر صادقة لشخص أثر في يومك"),
    ("Ten minutes of sunlight and unhurried tea", "عشر دقائق من ضوء الصباح وكوب دافئ"),
    ("Declutter one small surface for mental clarity", "ترتيب ركن صغير يمنح ذهنك صفاءً فورياً"),
    ("Read four pages of an inspiring book before noon", "قراءة أربع صفحات ملهمة قبل الظهيرة"),
    ("Walk mindfully without checking your phone", "مشي هادئ لعشر دقائق بعيداً عن الشاشات"),
    ("Sketch or doodle one botanical leaf in the margin", "رسم ورقة نباتية بسيطة لتنشيط الخيال"),
    ("Drink a full glass of water before starting deep work", "شرب كوب ماء كامل قبل بدء العمل العميق"),
    ("Celebrate one small win you overlooked this week", "الاحتفاء بإنجاز صغير أنجزته بهدوء"),
    ("Listen to soothing natural rain or birdsong while planning", "التخطيط على صوت الطبيعة وهدوء الصباح"),
    ("Pause at midday to stretch your shoulders and neck", "وقفة تمدد خفيفة في منتصف النهار"),
    ("Write down one curiosity question to explore", "تدوين سؤال فضولي واحد تود استكشافه"),
    ("Share an encouraging word with a colleague or family member", "كلمة طيبة ومشجعة لأحد من حولك"),
    ("Prepare tomorrow's top priority before closing your desk", "تحديد أهم أولوية للغد قبل ختام اليوم"),
    ("Notice three subtle colors in the sky this afternoon", "تأمل ثلاثة ألوان هادئة في السماء اليوم"),
    ("Protect a 45-minute uninterrupted single-task block", "حماية ٤٥ دقيقة من التركيز العميق دون مقاطعة"),
    ("Savor a healthy snack slowly and mindfully", "تذوق وجبة خفيفة صحية بوعي وامتنان"),
    ("Capture one favorite quote that resonates today", "تدوين اقتباس واحد لامس قلبك وفكرك"),
    ("Forgive one minor delay and reset calmly", "تجاوز تعثر بسيط والبدء من جديد بسلام"),
    ("Water a plant or notice nature around your home", "العناية بنبتة خضراء أو تأمل جمال الطبيعة"),
    ("Turn off notifications for one peaceful evening hour", "إيقاف الإشعارات لساعة مسائية هادئة"),
    ("Jot down three ideas without judging any of them", "كتابة ثلاث أفكار إبداعية بلا نقد مسبق"),
    ("Smile intentionally before beginning your hardest task", "ابتسامة هادئة قبل البدء بالمهمة الأهم"),
    ("End the workday by closing open loops on paper", "تفريغ المهام العالقة على الورق لراحة البال"),
];

const EVENING_REFLECTIONS: [(&str, &str); 16] = [
    ("What quiet moment brought you unexpected peace today?", "ما اللحظة الهادئة التي منحتك سكينة غير متوقعة اليوم؟"),
    ("Which single action moved your meaningful goal forward?", "ما الخطوة الواحدة التي قربتك من هدفك الأهم اليوم؟"),
    ("Who made your day lighter, and how can you thank them?", "من الذي خفف عنك عبء اليوم وكيف تعبر له عن امتنانك؟"),
    ("What lesson would you gently carry into tomorrow?", "ما الدرس اللطيف الذي تحمله معك إلى الغد؟"),
    ("Where did you show patience instead of rushing?", "أين تحليت بالصبر والروية بدلاً من الاستعجال؟"),
    ("What drained your energy, and what restored it?", "ما الذي استنزف طاقتك اليوم وما الذي أعاد شحنها؟"),
    ("Name three small blessings visible around you right now.", "اذكر ثلاث نعم صغيرة تراها حولك في هذه اللحظة."),
    ("What courageous boundary or 'no' protected your focus?", "ما القرار الواعي الذي حمى وقتك وتركيزك اليوم؟"),
    ("Which habit felt slightly easier today than last week?", "أي عادة إيجابية أصبحت أيسر عليك اليوم من الأسبوع الماضي؟"),
    ("What idea sparked your curiosity or creative joy?", "ما الفكرة التي أيقظت فضولك أو بهجتك الإبداعية؟"),
    ("If today were a chapter title, what would you name it?", "لو كان هذا اليوم عنوان فصل في كتابك، فماذا تسميه؟"),
    ("How did you care for your body and rest today?", "كيف اعتنيت بصحتك الجسدية وراحتك اليوم؟"),
    ("What worry can you safely release onto paper tonight?", "ما القلق الذي يمكنك تركه على الورق الليلة لتنام مطمئناً؟"),
    ("What beauty did you notice in an ordinary routine?", "ما الجمال الذي لاحظته في تفاصيلك اليومية المعتادة؟"),
    ("Which skill or virtue did you practice today?", "ما المهارة أو القيمة الجميلة التي مارستها اليوم؟"),
    ("What is one thing you are proud of completing today?", "ما الإنجاز الواحد الذي تفخر بإتمامه قبل ختام يومك؟"),
];

pub(crate) fn is_stationery_or_planner_prompt(prompt: &str) -> bool {
    let norm = normalize_curriculum_query(prompt);
    [
        "بلانر",
        "مفكره",
        "اجنده",
        "يوميات",
        "دفتر ملاحظات",
        "كورنيل",
        "بولت جورنال",
        "متتبع عادات",
        "planner",
        "journal",
        "journaling",
        "notebook",
        "agenda",
        "diary",
        "bujo",
        "bullet journal",
        "habit tracker",
        "gratitude",
        "scrapbook",
        "stationery",
        "365 page",
        "365 يوم",
        "365 صفحه",
    ]
    .iter()
    .any(|kw| norm.contains(kw))
}

pub(crate) fn is_planner_blueprint_target(prompt: &str, skill: &SkillSnapshot) -> bool {
    skill.skill_id == "planner-journal-studio-style" || is_stationery_or_planner_prompt(prompt)
}

pub(crate) fn season_for_page(page: u32, total_pages: u32) -> &'static SeasonTheme {
    let h = page.saturating_add(1);
    let idx = if total_pages >= 300 {
        if h <= 12 {
            0
        } else if h <= 348 {
            usize::try_from((h - 13) / 28).unwrap_or(0).min(11)
        } else {
            11
        }
    } else if total_pages > 1 {
        usize::try_from(u64::from(page).saturating_mul(12) / u64::from(total_pages)).unwrap_or(0).min(11)
    } else {
        0
    };
    SEASONS.get(idx).unwrap_or(&SEASONS[0])
}

/// Returns a canonical seasonal motif prompt when `SmartHybrid` can share one
/// monthly watercolor botanical vignette across recurring daily/weekly/notes pages.
pub(crate) fn smart_hybrid_seasonal_motif_prompt(skill_id: &str, family_id: &str, slot_id: &str, page: u32, total_pages: u32) -> Option<String> {
    let is_recurring_planner_slot = skill_id == "planner-journal-studio-style"
        && matches!(
            (family_id, slot_id),
            ("daily-sanctuary", "daily-botanical") | ("weekly-compass", "weekly-accent") | ("cornell-dot-notes", "notes-motif")
        );
    if !is_recurring_planner_slot {
        return None;
    }
    let season = season_for_page(page, total_pages);
    Some(format!(
        "[SEASONAL_MOTIF:M{:02}:{slot_id}] Delicate hand-painted pastel watercolor botanical corner vignette for {} ({}) featuring {}, isolated on pure warm paper white (#FFFFFF) with airy margins, zero text, zero letters, zero numbers, zero borders.",
        season.month, season.name_en, season.mood_en, season.botanical_motif
    ))
}

fn resolve_family<'a>(preferred: &'a str, declared: &[&'a str], fallback_idx: usize) -> &'a str {
    if let Some(&exact) = declared.iter().find(|&&id| id == preferred) {
        return exact;
    }
    if declared.is_empty() {
        return preferred;
    }
    let idx = fallback_idx.checked_rem(declared.len()).unwrap_or(0);
    declared.get(idx).copied().unwrap_or(preferred)
}

fn describe_planner_page(page: u32, total_pages: u32, declared: &[&str]) -> (String, usize, String, String, String, String) {
    let h = page.saturating_add(1);
    let season = season_for_page(page, total_pages);

    // Compact showcase / prototype planners (1..=8 pages).
    if total_pages <= 8 {
        let showcase = [
            ("planner-cover", "Front Cover & Sanctuary Identity / غلاف المفكرة وهوية العام", "Cover Title, Subtitle, Watercolor Wreath & Year"),
            (
                "ownership-dream-wheel",
                "Ownership Card & 8-Spoke Wheel of Dreams / صفحة الملكية وعجلة الأحلام",
                "Belongs-To Card, North Star Vision & 8 Life Balance Spokes",
            ),
            (
                "monthly-garden",
                "Monthly Botanical Garden & Habit Seedlings / حديقة الشهر ومتتبع العادات",
                "Monthly Goals, Key Dates & 5 Daily Habit Rows",
            ),
            (
                "weekly-compass",
                "Weekly Starlight Compass & Top Priorities / بوصلة الأسبوع والأولويات",
                "7-Day Rhythm Overview, Top 3 Focus & Energy Check",
            ),
            (
                "daily-sanctuary",
                "Daily Focus, Micro-Joy & Evening Gratitude / واحة التخطيط اليومي والامتنان",
                "Top 3 Must-Dos, Time-Blocked Schedule, Water Drops & Gratitude",
            ),
            (
                "creative-escape",
                "Weekly Creative Escape: Botanical Mandala & Brain Play / واحة الترفيه الإبداعي",
                "Coloring Mandala / Blossom Logic Puzzle & Mindful Reset",
            ),
            (
                "cornell-dot-notes",
                "Cornell Idea Studio & Dot-Grid Synthesis / استوديو الأفكار وملاحظات كورنيل",
                "Cue Column, Structured Notes & Bottom Takeaway Summary",
            ),
            (
                "polaroid-scrapbook",
                "Monthly Polaroid Memories & Gratitude Harvest / ألبوم الذكريات وحصاد الشهر",
                "3 Tilted Polaroid Frames, Captions & Month Highlights",
            ),
        ];
        let idx = usize::try_from(page).unwrap_or(0).min(showcase.len().saturating_sub(1));
        let (pref, label, stage) = showcase.get(idx).copied().unwrap_or(showcase[0]);
        let fam = resolve_family(pref, declared, idx);
        return (
            fam.to_owned(),
            1,
            label.to_owned(),
            stage.to_owned(),
            format!("Palette: {} | Botanical motif: {}", season.palette_cue, season.botanical_motif),
            "Keep generous writing space, crisp vector hairlines, and zero text inside raster watercolor vignettes.".to_owned(),
        );
    }

    // Full Year-Round Wonderland Planner (180..=1000 pages, canonical 365-page edition).
    if total_pages >= 180 {
        if h == 1 {
            let fam = resolve_family("planner-cover", declared, 0);
            return (
                fam.to_owned(),
                1,
                "Page 001 — Master Edition Front Cover & Botanical Medallion / غلاف مفكرة العام الشاملة".to_owned(),
                "Hero Title, Bilingual Subtitle, Seasonal Watercolor Medallion & Edition Year".to_owned(),
                format!("Opening palette: {} ({})", season.palette_cue, season.mood_en),
                "Transition into the 11-page Front-Matter Wonderland (Ownership, Dream Wheel, Yearly Constellation & Rituals).".to_owned(),
            );
        }
        if (2..=12).contains(&h) {
            let front_specs: [(&str, &str, &str); 11] = [
                (
                    "ownership-dream-wheel",
                    "Ownership Sanctuary & 8-Spoke Wheel of Dreams / ملكية المفكرة وعجلة توازن الحياة",
                    "Personal Dedication, Word of the Year & 8 Life Domains",
                ),
                (
                    "monthly-garden",
                    "Yearly Constellation & 12-Month Milestone Map / خريطة العام ومعالم الأشهر الاثني عشر",
                    "12-Month High-Level Milestones & Seasonal Anchor Dates",
                ),
                (
                    "ownership-dream-wheel",
                    "Core Values Compass & 5-Pillar Vision / بوصلة القيم الكبرى ورؤية العام",
                    "Guiding Principles, Non-Negotiables & Personal Mission",
                ),
                (
                    "cornell-dot-notes",
                    "Reading Wonderland: Bookshelf & Wisdom Log / مكتبة العام: كتب وقراءات ملهمة",
                    "20 Book Spine Titles, Reading Notes & Key Takeaways",
                ),
                (
                    "cornell-dot-notes",
                    "Cinema, Documentaries & Podcast Inspirations / واحة الإلهام السمعي والبصري",
                    "Curated Watchlist, Ideas Sparked & Creative Notes",
                ),
                (
                    "polaroid-scrapbook",
                    "Dream Board & Visual Intention Gallery / لوحة الرؤية والأحلام البصرية",
                    "3 Vision Polaroid Frames, Affirmations & Future Snapshots",
                ),
                (
                    "weekly-compass",
                    "Ideal Morning & Evening Sanctuary Rituals / هندسة الروتين الصباحي والمسائي",
                    "Energizing Morning Anchor, Deep Work Window & Calm Wind-Down",
                ),
                (
                    "monthly-garden",
                    "Annual Vitality, Sleep & Mindful Habit Matrix / مصفوفة العافية والعادات السنوية",
                    "Keystone Habits, Rest Rhythm & Gentle Self-Care",
                ),
                (
                    "weekly-compass",
                    "Mindful Abundance, Savings & Generosity Plan / خطة الادخار والوفرة والعطاء",
                    "Financial Clarity Goals, Saving Milestones & Giving Log",
                ),
                (
                    "creative-escape",
                    "Opening Botanical Mandala Coloring Sanctuary / واحة تلوين الماندالا الافتتاحية",
                    "Intricate Line-Art Floral Mandala & Intention Breathing",
                ),
                (
                    "cornell-dot-notes",
                    "Time-Capsule Letter to My Future Self / رسالة إلى ذاتي في اليوم ٣٦٥",
                    "Heartfelt Letter, Hopes & Sealed Reflection Summary",
                ),
            ];
            let f_idx = usize::try_from(h - 2).unwrap_or(0).min(10);
            let (pref, label, stage) = front_specs[f_idx];
            let fam = resolve_family(pref, declared, f_idx + 1);
            return (
                fam.to_owned(),
                1,
                format!("Page {h:03} — {label}"),
                stage.to_owned(),
                "Front-Matter Wonderland (Pages 002–012): set intentional foundations before Month 1 begins on Page 013.".to_owned(),
                "Maintain airy cream/white writing zones and distinct topical copy on every front-matter page.".to_owned(),
            );
        }

        let months_end = 12u32.saturating_add(12 * 28).min(total_pages);
        if h >= 13 && h <= months_end {
            let offset = h - 13;
            let month_no = usize::try_from(offset / 28).unwrap_or(0).min(11);
            let in_month = offset % 28;
            let s = &SEASONS[month_no];
            let unit_no = month_no + 1;
            let (pref, label, stage, cue) = match in_month {
                0 => (
                    "monthly-garden",
                    format!("Page {h:03} — {} ({}) Monthly Garden & Habit Seedlings / حديقة {} ومتتبع العادات", s.name_en, s.mood_en, s.name_ar),
                    "Monthly Theme Opener, Top 3 Month Goals & 5 Habit Tracker Rows".to_owned(),
                    format!("Season {}: {} | Motif: {}", s.month, s.palette_cue, s.botanical_motif),
                ),
                1 | 7 | 13 | 19 => {
                    let wk = (in_month / 6) + 1;
                    (
                        "weekly-compass",
                        format!("Page {h:03} — {} Week {wk} Starlight Compass / بوصلة الأسبوع {wk} لشهر {}", s.name_en, s.name_ar),
                        format!("Week {wk} Priorities, 7-Day Rhythm Cards, Habit Focus & Weekly Win"),
                        format!("Season {}: {} — Week {wk} of 4", s.month, s.mood_ar),
                    )
                }
                6 | 12 | 18 | 24 => {
                    let wk = (in_month / 6).max(1);
                    let escapes = [
                        (
                            "Botanical Mandala Coloring Sanctuary / واحة تلوين الماندالا النباتية",
                            "Crisp black-and-white botanical mandala line art on pure white background for coloring + calm reflection",
                        ),
                        (
                            "4×4 Blossom Brain Sudoku & Logic Reset / سودوكو الأزهار وتنشيط الذهن",
                            "Playful 4x4 symbol logic puzzle + creative lateral-thinking prompt",
                        ),
                        (
                            "9-Square Joy & Kindness Bingo / بينغو البهجة واللطف الأسبوعي",
                            "9 gentle real-life micro-adventure prompts + joyful illustration",
                        ),
                        (
                            "Mindful Botanical Doodle & Zentangle Studio / مساحة الخربشة الإبداعية والتأمل",
                            "Guided botanical step-by-step doodle prompt + open sketching sanctuary",
                        ),
                    ];
                    let e_idx = usize::try_from((wk - 1) % 4).unwrap_or(0);
                    let (e_title, e_desc) = escapes[e_idx];
                    (
                        "creative-escape",
                        format!("Page {h:03} — {} Week {wk} Creative Escape: {e_title}", s.name_en),
                        e_desc.to_owned(),
                        format!("Weekly screen-free entertainment break ({})", s.botanical_motif),
                    )
                }
                25 => (
                    "cornell-dot-notes",
                    format!("Page {h:03} — {} Cornell Idea Studio & Deep Notes / استوديو أفكار {}", s.name_en, s.name_ar),
                    "Cornell Cue Margin, Ruled Synthesis Notes & Bottom Summary Box".to_owned(),
                    format!("{} intellectual & creative capture page", s.name_en),
                ),
                26 => (
                    "cornell-dot-notes",
                    format!("Page {h:03} — {} Project Sketch & Brainstorm Notes / تخطيط مشاريع {}", s.name_en, s.name_ar),
                    "Structured Action Cues, Brainstorm Lines & Next-Step Takeaway".to_owned(),
                    format!("{} freeform project synthesis", s.name_en),
                ),
                27 => (
                    "polaroid-scrapbook",
                    format!("Page {h:03} — {} Polaroid Memories & Gratitude Harvest / ألبوم ذكريات {}", s.name_en, s.name_ar),
                    "3 Monthly Memory Frames, Handwritten-Style Captions & Month Harvest".to_owned(),
                    format!("Closing celebration of {} ({}) before next month", s.name_en, s.name_ar),
                ),
                _ => {
                    let wk = (in_month / 6) + 1;
                    let day_in_wk = in_month - ((wk - 1) * 6 + 1);
                    let day_seq = usize::try_from(month_no as u32 * 16 + (wk - 1) * 4 + day_in_wk).unwrap_or(1);
                    let (joy_en, joy_ar) = MICRO_JOYS[day_seq % MICRO_JOYS.len()];
                    let (ref_en, ref_ar) = EVENING_REFLECTIONS[(day_seq.wrapping_mul(7) + month_no) % EVENING_REFLECTIONS.len()];
                    (
                        "daily-sanctuary",
                        format!("Page {h:03} — {} Focus Day {day_seq} (Week {wk} · Day {day_in_wk}) / واحة اليوم — {}", s.name_en, s.name_ar),
                        format!("Top 3 Priorities, 06:00–21:00 Schedule, Hydration Drops | Micro-Joy: {joy_en} ({joy_ar})"),
                        format!("Unique Evening Reflection Prompt: \"{ref_en}\" / «{ref_ar}»"),
                    )
                }
            };
            let fam = resolve_family(pref, declared, usize::try_from(in_month).unwrap_or(0));
            return (
                fam.to_owned(),
                unit_no,
                label,
                stage,
                cue,
                "Never repeat yesterday's micro-joy or reflection prompt; keep writing lines spacious and legible.".to_owned(),
            );
        }

        // Back-Matter Reflection & Keepsake Vault (Pages 349..=total_pages).
        let back_idx = usize::try_from(h.saturating_sub(months_end + 1)).unwrap_or(0);
        let back_specs: [(&str, &str, &str); 6] = [
            (
                "ownership-dream-wheel",
                "Year-in-Review Fulfilment Wheel / عجلة حصاد العام وتقييم التوازن",
                "Compare Start-of-Year Dreams with End-of-Year Growth Across 8 Domains",
            ),
            (
                "polaroid-scrapbook",
                "Unforgettable Moments & Golden Memories / أجمل لحظات العام وذكرياته الذهبية",
                "3 Keepsake Polaroid Frames, Favorite Trips, Friendships & Milestones",
            ),
            (
                "cornell-dot-notes",
                "12 Lessons This Year Taught Me / اثنا عشر درساً علمني إياها هذا العام",
                "Wisdom Gained, Habits Mastered & Gratitude Synthesis",
            ),
            (
                "creative-escape",
                "Year-End Celebration Mandala & Mindful Closure / ماندالا ختام العام والاحتفال بالأثر",
                "Celebratory Botanical Mandala Line-Art & Peaceful Closure",
            ),
            (
                "cornell-dot-notes",
                "Seeds & Ideas for the Coming Year / بذور الأحلام والمشاريع للعام القادم",
                "Future Projects, Skills to Learn & Gentle Aspirations",
            ),
            (
                "polaroid-scrapbook",
                "Final Keepsake Colophon & Gratitude Seal / مسك الختام وختم الامتنان السنوي",
                "Closing Tribute, Signature Date & Completed Planner Keepsake",
            ),
        ];
        let (pref, label, stage) = back_specs[back_idx % back_specs.len()];
        let fam = resolve_family(pref, declared, back_idx);
        return (
            fam.to_owned(),
            12,
            format!("Page {h:03} — {label}"),
            stage.to_owned(),
            "Back-Matter Keepsake Archive: warm closure, reflection, and next-year inspiration.".to_owned(),
            "Preserve calm margins and celebratory botanical elegance.".to_owned(),
        );
    }

    // Medium Thematic / Monthly / 30–90-Day Planner (9..=179 pages).
    let (pref, label, stage) = if h == 1 {
        ("planner-cover", format!("Page {h:02} — Thematic Planner Cover / غلاف المفكرة"), "Cover Title, Subtitle, Botanical Hero & Edition")
    } else if h == 2 {
        (
            "ownership-dream-wheel",
            format!("Page {h:02} — Ownership & Intention Wheel / الملكية وعجلة الأهداف"),
            "Owner Card, Core Intention & 8-Spoke Goal Wheel",
        )
    } else if h == 3 {
        (
            "monthly-garden",
            format!("Page {h:02} — Master Calendar & Habit Matrix / خريطة الشهر ومتتبع العادات"),
            "30-Day Overview & 5 Keystone Habit Trackers",
        )
    } else if h == total_pages {
        (
            "polaroid-scrapbook",
            format!("Page {h:02} — Closing Memories & Harvest / حصاد الرحلة وألبوم الذكريات"),
            "3 Highlight Frames, Gratitude Harvest & Next Chapter",
        )
    } else {
        let cycle = (h - 4) % 7;
        let wk = ((h - 4) / 7) + 1;
        match cycle {
            0 => (
                "weekly-compass",
                format!("Page {h:02} — Week {wk} Compass & Priorities / بوصلة الأسبوع {wk}"),
                "Weekly Top 3, 7-Day Rhythm & Energy Focus",
            ),
            5 => (
                "creative-escape",
                format!("Page {h:02} — Week {wk} Creative Escape / واحة الإبداع الأسبوعية {wk}"),
                "Mindful Coloring / Puzzle Break & Joy Prompt",
            ),
            6 => (
                "cornell-dot-notes",
                format!("Page {h:02} — Week {wk} Cornell Notes & Reflection / ملاحظات ومراجعة الأسبوع {wk}"),
                "Key Insights, Ideas & Summary Box",
            ),
            d => {
                let day_no = usize::try_from((wk - 1) * 4 + d).unwrap_or(1);
                let (joy_en, _) = MICRO_JOYS[day_no % MICRO_JOYS.len()];
                let _ = joy_en;
                (
                    "daily-sanctuary",
                    format!("Page {h:02} — Day {day_no} Focus & Gratitude Sanctuary / واحة التخطيط لليوم {day_no}"),
                    "Top 3 Must-Dos, Time Schedule, Micro-Joy & Evening Gratitude",
                )
            }
        }
    };
    let fam = resolve_family(pref, declared, usize::try_from(page).unwrap_or(0));
    (
        fam.to_owned(),
        usize::from(season.month),
        label,
        stage.to_owned(),
        format!("Palette: {} | Motif: {}", season.palette_cue, season.botanical_motif),
        "Ensure each day/week carries distinct prompts, priorities, and gratitude reflections.".to_owned(),
    )
}

pub(crate) fn planner_blueprint(prompt: &str, skill: &SkillSnapshot, scope: BatchScope, total_pages: u32) -> Result<Value, String> {
    let profile: &DesignProfile = profile(skill)?;
    let end = scope_end(scope)?;
    let declared_families: Vec<&str> = profile.page_families.iter().map(|f| f.id.as_str()).collect();
    let topic_focus = extract_prompt_topic(prompt);
    let has_arabic = prompt.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c) && c.is_alphabetic());
    let has_latin = prompt.chars().any(|c| c.is_ascii_alphabetic());
    let language_mode = if has_arabic && has_latin {
        "Bilingual Stationery (Arabic & English harmonious editorial labels)"
    } else if has_arabic {
        "Arabic primary stationery (logical RTL typography, crisp harakat on titles/quotes, clean numerals)"
    } else {
        "English primary stationery"
    };

    let mut scoped_page_plan = Vec::with_capacity(usize::try_from(scope.count).unwrap_or(0));
    for page in scope.start..end {
        let (recommended_family, month_unit, chapter_label, stage, palette_cue, bridge) =
            describe_planner_page(page, total_pages, &declared_families);
        scoped_page_plan.push(json!({
            "page": page,
            "recommended_family": recommended_family,
            "display_archetype": "مفكرة وبلانر إبداعي متكامل | Bespoke Planner & Journal Sanctuary",
            "unit_number": month_unit,
            "chapter_and_lesson": chapter_label,
            "cognitive_stage": stage,
            "mental_chain": {
                "prerequisite_from_previous_page": palette_cue,
                "intra_page_flow": "Header pill & date anchor -> Primary vector tracker / schedule grid -> Micro-joy or creative prompt -> Calm reflection footer inside 12mm safe margins.",
                "spiral_bridge_to_next_page": bridge
            }
        }));
    }

    let seasons_json: Vec<Value> = SEASONS
        .iter()
        .map(|s| {
            json!({
                "unit": s.month,
                "title": format!("Month {:02}: {} — {} / {} — {}", s.month, s.name_en, s.mood_en, s.name_ar, s.mood_ar),
                "palette": s.palette_cue,
                "botanical_motif": s.botanical_motif
            })
        })
        .collect();

    let family_slot_max_chars: BTreeMap<&str, BTreeMap<&str, usize>> = profile
        .page_families
        .iter()
        .map(|family| {
            let slots = family.text_slots.iter().map(|slot| (slot.id.as_str(), slot.max_chars)).collect();
            (family.id.as_str(), slots)
        })
        .collect();

    Ok(json!({
        "outline_status": "Master Stationery & 365-Day Planner Architecture (non-repeating seasonal progression)",
        "series_identity_policy": "Create original bespoke stationery titles, micro-joy prompts, habit trackers, creative escapes, and reflection questions tailored to the user's request.",
        "family_slot_max_chars": family_slot_max_chars,
        "selected_profile_authority": {
            "style_id": skill.skill_id,
            "name": profile.name,
            "page_mm": profile.page_mm,
            "margin_mm": profile.margin_mm,
            "palette": profile.palette,
            "typography": profile.typography,
            "layout": profile.layout,
            "illustration": profile.illustration
        },
        "subject_domain": "Planners, Guided Journals, Creative Escapes & Executive Notebooks (استوديو البلانرات والمفكرات الإبداعية الشاملة)",
        "prompt_topic_focus": topic_focus,
        "language_and_script_mode": language_mode,
        "suggested_methodology": "Balanced stationery rhythm: Intentional Front-Matter -> 12 Seasonal Months (Monthly Garden + Weekly Compasses + Non-Repeating Daily Sanctuaries + Weekly Creative Escapes + Cornell Notes + Polaroid Scrapbook) -> Year-End Reflection Keepsakes.",
        "units_and_chapters": seasons_json,
        "scoped_page_plan": scoped_page_plan,
        "stationery_prepress_rules": {
            "native_grids": "All writing rules, dot grids, habit checkboxes, schedules, and Polaroid frames are crisp editable native vector geometry.",
            "raster_text_ban": "Strictly NO text, NO letters, NO numbers, and NO speech bubbles inside watercolor illustrations."
        }
    }))
}
