//! Curriculum subject detection, Arabic query normalization, blueprint sequencing, and unit tracking.
use super::counting::{bindable_counting_family, counting_family};
use super::{profile, scope_end};
use crate::worker::BatchScope;
use designcraft_ai_skills::SkillSnapshot;
use designcraft_ai_types::protocol::MAX_BRIEF_BYTES;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CurriculumSubject {
    IslamicStudies,
    ArabicLanguage,
    SocialStudies,
    Mathematics,
    ComputerScience,
    Science,
    Languages,
    Storybook,
    Magazine,
    Brochure,
    Report,
    Flashcards,
    ActivityWorkbook,
    PosterOrCertificate,
    Integrated,
}

/// Extracts a concise human-readable topic phrase from an open-ended user prompt
/// so custom subjects and publications reflect the user's actual request.
pub(crate) fn extract_prompt_topic(prompt: &str) -> String {
    let first_line = prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or(prompt).trim();
    let stop_words = [
        "اريد",
        "أريد",
        "ابي",
        "أبي",
        "ابغى",
        "أبغى",
        "صمم",
        "انتج",
        "أنتج",
        "انشئ",
        "أنشئ",
        "اعمل",
        "لي",
        "لنا",
        "عن",
        "في",
        "باسلوب",
        "بأسلوب",
        "من",
        "صفحات",
        "صفحة",
        "صفحه",
        "please",
        "create",
        "design",
        "produce",
        "make",
        "build",
        "generate",
    ];
    let cleaned: Vec<&str> = first_line
        .split_whitespace()
        .filter(|w| {
            let trimmed = w.trim_matches(|c: char| !c.is_alphanumeric());
            !trimmed.is_empty() && !stop_words.contains(&trimmed.to_lowercase().as_str())
        })
        .take(10)
        .collect();
    let candidate = if cleaned.is_empty() { first_line.to_owned() } else { cleaned.join(" ") };
    let chars: String = candidate.chars().take(72).collect();
    if chars.trim().is_empty() { "الموضوع المخصص / Custom Publication".into() } else { chars }
}

/// Normalize mixed Arabic/Latin prompts (such as "اريد منهج دولي لمادةmath" or "لماده الاسلاميه لمرحله kg2")
/// by separating script transitions, stripping diacritics, and unifying Arabic alef/ta-marbuta variants.
pub(crate) fn normalize_curriculum_query(text: &str) -> String {
    let bounded_cap = text.len().min(32_768).saturating_add(16);
    let mut out = String::with_capacity(bounded_cap);
    let mut prev_script: u8 = 0; // 1 = Arabic, 2 = ASCII alphanumeric
    for c in text.chars().take(32_768) {
        if matches!(c, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}') {
            continue;
        }
        let c = match c {
            'أ' | 'إ' | 'آ' | 'ٱ' => 'ا',
            'ة' => 'ه',
            'ى' => 'ي',
            other => other,
        };
        let cur_script = if ('\u{0600}'..='\u{06ff}').contains(&c) {
            1
        } else if c.is_ascii_alphanumeric() {
            2
        } else {
            0
        };
        if prev_script != 0 && cur_script != 0 && prev_script != cur_script {
            out.push(' ');
        }
        for lower in c.to_lowercase() {
            out.push(lower);
        }
        prev_script = cur_script;
    }
    out
}

fn token_matches_query(token: &str, target: &str) -> bool {
    if token == target {
        return true;
    }
    if target.chars().count() >= 3 {
        let stripped_target = target.strip_prefix("ال").unwrap_or(target);
        if token == stripped_target || token.strip_prefix("ال") == Some(stripped_target) {
            return true;
        }
        for prefix in ["وال", "بال", "لال", "كال", "فال", "لل", "و", "ف"] {
            if let Some(rest) = token.strip_prefix(prefix)
                && (rest == target || rest == stripped_target)
            {
                return true;
            }
        }
        if stripped_target.chars().count() >= 5 {
            for prefix in ["ب", "ل", "ك"] {
                if let Some(rest) = token.strip_prefix(prefix)
                    && (rest == target || rest == stripped_target)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn contains_query_phrase(tokens: &[&str], phrase: &str) -> bool {
    let norm_phrase = normalize_curriculum_query(phrase);
    let q_tokens: Vec<&str> = norm_phrase.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if q_tokens.is_empty() || q_tokens.len() > tokens.len() {
        return false;
    }
    tokens.windows(q_tokens.len()).any(|win| win.iter().zip(&q_tokens).all(|(tok, qt)| token_matches_query(tok, qt)))
}

pub(crate) fn detect_curriculum_subject(prompt: &str) -> CurriculumSubject {
    let norm = normalize_curriculum_query(prompt);
    let tokens: Vec<&str> = norm.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |terms: &[&str]| terms.iter().any(|t| contains_query_phrase(&tokens, t));
    let has_word = |word: &str| tokens.contains(&word);

    // Strip combining marks only (preserving hamza-above vs hamza-below) to distinguish
    // `أعداد` (numbers) from `إعداد` (preparation / authoring).
    let hamza_preserved: String = prompt
        .chars()
        .take(32_768)
        .filter(|&c| !matches!(c, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}'))
        .collect();
    let has_explicit_numbers_a3dad =
        hamza_preserved.split(|c: char| !c.is_alphanumeric()).any(|w| matches!(w, "أعداد" | "الأعداد" | "والأعداد" | "بالأعداد" | "للأعداد"));
    let has_prep_i3dad =
        hamza_preserved.split(|c: char| !c.is_alphanumeric()).any(|w| matches!(w, "إعداد" | "الإعداد" | "وإعداد" | "لإعداد" | "بإعداد"));
    let has_unhamzated_a3dad = !has_prep_i3dad && has(&["اعداد"]);

    let explicit_arabic_lang = has(&[
        "arabic language",
        "arabic literacy",
        "لغتي",
        "اللغه العربيه",
        "لغه عربيه",
        "حروف الهجاء",
        "ابجديه عربيه",
        "نحو",
        "صرف",
        "املاء",
        "الخط العربي",
        "المفرد والجمع",
        "مفرد وجمع",
        "جمع المذكر",
        "جمع المؤنث",
        "جمع التكسير",
    ]) && !has(&["english", "انجليزي", "انكليزي"]);

    let grammatical_or_info_jam3 =
        has(&["المفرد والجمع", "مفرد وجمع", "جمع المذكر", "جمع المؤنث", "جمع التكسير", "جمع الكلمات", "جمع المعلومات", "جمع البيانات", "جمع الافكار"]);
    let rhetorical_tarh = has(&["طرح الاسئله", "طرح سؤال", "طرح الافكار", "طرح الموضوع", "طرح الاراء"]);
    let student_metadata_bayanat =
        has(&["بيانات الطالب", "بيانات التلميذ", "بيانات الكتاب", "بيانات المدرسه", "بيانات المعلم", "بيانات شخصيه", "بطاقه البيانات"]);

    let strong_math = has(&[
        "math",
        "maths",
        "mathematics",
        "arithmetic",
        "geometry",
        "algebra",
        "counting",
        "subitiz",
        "subitizing",
        "number bond",
        "number bonds",
        "ten frame",
        "five frame",
        "رياضيات",
        "حساب",
        "جبر",
        "هندسه",
        "ارقام",
        "العد",
        "كميات",
        "قسمه",
        "ضرب",
    ]) || has_explicit_numbers_a3dad
        || (has_unhamzated_a3dad && !explicit_arabic_lang);

    let contextual_math =
        (!grammatical_or_info_jam3 && !explicit_arabic_lang && has(&["جمع"])) || (!rhetorical_tarh && !explicit_arabic_lang && has(&["طرح"]));

    let strong_cs = has(&[
        "computer",
        "computing",
        "ict",
        "coding",
        "programming",
        "algorithm",
        "algorithms",
        "robotics",
        "software",
        "hardware",
        "حاسوب",
        "كمبيوتر",
        "برمجه",
        "خوارزميات",
        "روبوت",
        "شبكات",
        "قواعد بيانات",
        "قاعده بيانات",
        "تحليل بيانات",
    ]);
    let contextual_cs = !student_metadata_bayanat && !explicit_arabic_lang && has(&["بيانات", "رقمي", "رقميه", "تقنيه المعلومات"]);

    if has_word("islam")
        || has(&[
            "islamic",
            "quran",
            "hadith",
            "seerah",
            "fiqh",
            "aqeedah",
            "tawheed",
            "azkar",
            "adhkar",
            "اسلاميه",
            "اسلامي",
            "الاسلاميه",
            "تربيه اسلاميه",
            "التربيه الاسلاميه",
            "قران",
            "حديث",
            "سيره",
            "عبادات",
            "توحيد",
            "عقيده",
            "اذكار",
            "اداب اسلاميه",
            "فقه",
            "اركان الاسلام",
            "الوضوء",
            "الصلاه",
        ])
    {
        CurriculumSubject::IslamicStudies
    } else if strong_math || contextual_math {
        CurriculumSubject::Mathematics
    } else if explicit_arabic_lang {
        CurriculumSubject::ArabicLanguage
    } else if strong_cs || contextual_cs {
        CurriculumSubject::ComputerScience
    } else if has(&[
        "science",
        "steam",
        "stem",
        "biology",
        "physics",
        "chemistry",
        "علوم",
        "طبيعه",
        "فيزياء",
        "كيمياء",
        "احياء",
        "نبات",
        "حيوان",
        "طاقه",
        "فضاء",
    ]) {
        CurriculumSubject::Science
    } else if has(&[
        "social studies",
        "civics",
        "citizenship",
        "geography",
        "اجتماعيات",
        "الدراسات الاجتماعيه",
        "دراسات اجتماعيه",
        "تربيه وطنيه",
        "وطنيه",
        "جغرافيا",
        "تاريخ",
        "حضاره",
    ]) {
        CurriculumSubject::SocialStudies
    } else if has(&[
        "english",
        "phonics",
        "grammar",
        "vocabulary",
        "reading",
        "literacy",
        "انجليزي",
        "انكليزي",
        "لغه",
        "قراءه",
        "صوتيات",
        "حروف",
        "مفردات",
        "قواعد",
    ]) {
        CurriculumSubject::Languages
    } else if has(&[
        "brochure",
        "pamphlet",
        "flyer",
        "leaflet",
        "catalog",
        "catalogue",
        "company profile",
        "بروشور",
        "مطويه",
        "كتالوج",
        "دليل تعريفي",
        "بروفايل",
        "عرض خدمات",
    ]) {
        CurriculumSubject::Brochure
    } else if has(&["magazine", "newsletter", "periodical", "journal", "editorial issue", "مجله", "نشره", "دوريه", "صحيفه"]) {
        CurriculumSubject::Magazine
    } else if has(&[
        "annual report",
        "technical report",
        "whitepaper",
        "case study",
        "executive summary",
        "تقرير",
        "دراسه",
        "ورقه عمل",
        "احصائيات",
        "مؤشرات اداء",
    ]) {
        CurriculumSubject::Report
    } else if has(&["poster", "certificate", "infographic", "diploma", "banner", "ملصق", "بوستر", "شهاده", "لوحه جداريه", "انفوجرافيك"])
    {
        CurriculumSubject::PosterOrCertificate
    } else if has(&["storybook", "picture book", "bedtime story", "fairy tale", "قصه مصوره", "قصه اطفال", "قصص", "حكايه", "روايه مصوره"])
    {
        CurriculumSubject::Storybook
    } else if has(&["flashcard", "flashcards", "study cards", "بطاقات", "فلاش كارد", "بطاقه تعليميه"]) {
        CurriculumSubject::Flashcards
    } else if has(&["workbook", "activity book", "worksheet", "worksheets", "كتاب انشطه", "كراسه تمارين", "دفتر عمل", "اوراق عمل", "تدريبات"])
    {
        CurriculumSubject::ActivityWorkbook
    } else {
        CurriculumSubject::Integrated
    }
}

fn parsed_grade_number(norm: &str) -> Option<u32> {
    let ascii_num = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                '\u{0660}'..='\u{0669}' => char::from_u32(u32::from('0') + (u32::from(c) - 0x0660)).unwrap_or(c),
                '\u{06F0}'..='\u{06F9}' => char::from_u32(u32::from('0') + (u32::from(c) - 0x06F0)).unwrap_or(c),
                _ => c,
            })
            .collect()
    };
    let words: Vec<&str> = norm.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    for (idx, word) in words.iter().enumerate() {
        if matches!(*word, "grade" | "صف" | "الصف" | "للصف" | "بالصف" | "والصف" | "فالصف" | "كالصف" | "لصف" | "بصف")
            && let Some(next) = words.get(idx.saturating_add(1))
        {
            if let Ok(num) = ascii_num(next).parse::<u32>() {
                return Some(num);
            }
            let two = words.get(idx.saturating_add(2)).copied().unwrap_or("");
            if two == "عشر" {
                if matches!(*next, "الحادي" | "حادي" | "احد" | "أحد") {
                    return Some(11);
                }
                if matches!(*next, "الثاني" | "ثاني" | "اثنا" | "اثني") {
                    return Some(12);
                }
            }
            let ord = match *next {
                "العاشر" | "عاشر" => Some(10),
                "الحادي" | "حادي" => Some(11),
                "التاسع" | "تاسع" => Some(9),
                "الثامن" | "ثامن" => Some(8),
                "السابع" | "سابع" => Some(7),
                "السادس" | "سادس" => Some(6),
                "الخامس" | "خامس" => Some(5),
                "الرابع" | "رابع" => Some(4),
                "الثالث" | "ثالث" => Some(3),
                "الثاني" | "ثاني" => Some(2),
                "الاول" | "اول" | "الأول" | "أول" => Some(1),
                _ => None,
            };
            if ord.is_some() {
                return ord;
            }
        }
    }
    None
}

pub(crate) fn infer_learner_stage(prompt: &str) -> &'static str {
    let norm = normalize_curriculum_query(prompt);
    let has = |terms: &[&str]| terms.iter().any(|t| norm.contains(&normalize_curriculum_query(t)));
    if has(&["kg 2", "kg2", "fs2", "روضه ثانيه", "الروضه الثانيه", "روضه 2", "الروضه 2", "المستوي الثاني للروضه", "تمهيدي متقدم"])
    {
        "KG2 / Early Years Foundation Stage 2 (Ages 5–6) / مرحلة الروضة الثانية (KG2)"
    } else if has(&["kg 1", "kg1", "fs1", "روضه اولي", "الروضه الاولي", "روضه 1", "الروضه 1", "المستوي الاول للروضه"])
    {
        "KG1 / Early Years Foundation Stage 1 (Ages 4–5) / مرحلة الروضة الأولى (KG1)"
    } else if has(&["secondary", "high school", "ثانوي", "ثانويه", "المرحله الثانويه"])
        || parsed_grade_number(&norm).is_some_and(|g| (10..=12).contains(&g))
    {
        "Secondary & High School Stage / المرحلة الثانوية"
    } else if has(&["middle school", "preparatory", "اعدادي", "متوسط", "المرحله الاعداديه", "المرحله المتوسطه"])
        || parsed_grade_number(&norm).is_some_and(|g| (7..=9).contains(&g))
    {
        "Middle & Preparatory Stage / المرحلة المتوسطة والإعدادية"
    } else if has(&["kg", "kindergarten", "preschool", "early years", "روضه", "الروضه", "تمهيدي", "اطفال", "للصغار"]) {
        "Early Years & Kindergarten (Ages 4–6) / مرحلة رياض الأطفال والتمهيدي"
    } else if has(&["primary", "elementary", "ابتدائي", "المرحله الابتدائيه"]) || parsed_grade_number(&norm).is_some_and(|g| (1..=6).contains(&g))
    {
        "Primary & Elementary Stage / المرحلة الابتدائية"
    } else {
        "Adaptive Learner & Reader Stage (inferred from brief and style profile)"
    }
}

/// Mastermind orchestration brief combining active skills, native layout tools, and spatial harmony rules.
pub(crate) fn skill_tool_mastermind_brief(
    prompt: &str,
    skills: &[SkillSnapshot],
    scope: BatchScope,
    total_pages: u32,
    explicit_rtl_binding: Option<bool>,
) -> Value {
    let subject = detect_curriculum_subject(prompt);
    let topic_focus = extract_prompt_topic(prompt);
    let rtl_primary = explicit_rtl_binding.unwrap_or_else(|| crate::facing_pages::auto_direction(prompt) == "rightToLeft");
    let stage = infer_learner_stage(prompt);
    let active_skills: Vec<Value> = skills
        .iter()
        .map(|s| {
            json!({
                "skill_id": s.skill_id,
                "revision": s.revision,
                "role": if s.design_profile.is_some() {
                    "Pinned Design Style & Page-Family Architecture"
                } else {
                    "Active Domain, Typography, Pedagogy & Preflight Skill"
                }
            })
        })
        .collect();
    json!({
        "engine_role": "Universal Skill & Native Tool Mastermind",
        "inferred_subject": format!("{subject:?}"),
        "prompt_topic_focus": topic_focus,
        "inferred_learner_stage": stage,
        "inferred_direction": if rtl_primary { "Right-to-Left (RTL) primary with bidirectional support" } else { "Left-to-Right (LTR) primary" },
        "page_scope": { "start": scope.start, "count": scope.count, "total_pages": total_pages },
        "active_skill_synergy": active_skills,
        "spatial_and_logical_harmony_rules": [
            "Zone 1 (Header & Identity): Keep unit badge, lesson title, and objective pill aligned at the top safe margin with clear visual hierarchy.",
            "Zone 2 (Discovery & Visual Anchor): Connect hero scenes, concept cards, or 4-panel stories directly to the lesson topic with generous breathing room.",
            "Zone 3 (Structured Analysis & Sequence): Use ordered cards, step flowcharts, or comparison tables with consistent gutters and logical reading order (right-to-left for Arabic, left-to-right for English/equations).",
            "Zone 4 (Interactive Student Task & Self-Check): Provide clear student response boxes, tracing/matching activities, and a calm footer with page numbering inside safe margins."
        ]
    })
}

fn family_display_archetype(subject: CurriculumSubject, family_id: &str) -> &'static str {
    match family_id {
        "international-curriculum-cover" | "cover" => "غلاف الإصدار وهوية السلسلة | Cover & Series Identity",
        "international-scope-sequence" | "curriculum-map" => "خريطة المحتوى وتسلسل الوحدات | Scope & Sequence Map",
        "observe-growth-sequence" => "مشهد الاستكشاف المتدرج | Guided Discovery Opener",
        "oxford-vocab-story" => match subject {
            CurriculumSubject::IslamicStudies => "بطاقات المفاهيم وقصة السيرة والآداب | Islamic Concept Cards & 4-Panel Moral Story",
            CurriculumSubject::ArabicLanguage => "مفردات الحروف المصورة وقصة القراءة | Illustrated Vocabulary & 4-Panel Arabic Story",
            CurriculumSubject::SocialStudies => "بطاقات الهوية والمجتمع والمشهد الحواري | Social Concept Cards & Community Story",
            CurriculumSubject::Storybook => "لوحات القصة المصورة والحوار السردي | Illustrated Storyboard & Narrative Panels",
            CurriculumSubject::Flashcards => "شبكة البطاقات التعليمية المصورة | Illustrated Flashcard Grid & Context",
            _ => "بطاقات المفاهيم المصورة والقصة التطبيقية | Concept Cards & 4-Panel Story",
        },
        "oxford-skills-time" => match subject {
            CurriculumSubject::IslamicStudies => "تلاوة وفهم السور القصيرة والحديث والاستماع | Surah/Hadith Reflection, Listening & Discussion",
            CurriculumSubject::ArabicLanguage => "القراءة الفاهمة والاستماع والتحدث بالعربية | Arabic Reading Comprehension, Listening & Talk",
            CurriculumSubject::SocialStudies => "نص استقصائي عن الوطن والمجتمع وحوار ثنائي | Civic Reading Passage & Guided Discussion",
            _ => "القراءة الفاهمة والاستماع والحوار التطبيقي | Reading Passage, Listening & Discussion",
        },
        "oxford-fluency-project" => match subject {
            CurriculumSubject::IslamicStudies => "الأذكار والآداب اليومية ومشروع العمل الصالح | Daily Adhkar, Sunnah Manners & Practical Project",
            CurriculumSubject::ArabicLanguage => "التعبير الشفهي ومشروع القصة والتقييم الذاتي | Oral Expression, Mini-Book Project & Self-Check",
            _ => "التطبيق الحواري والمشروع العملي والتقييم الذاتي | Practical Dialogue, 3-Step Project & Self-Check",
        },
        "oxford-song-colours" => match subject {
            CurriculumSubject::IslamicStudies => "أنشودة القيم الإسلامية وتصنيف المفاهيم | Islamic Values Chant & Visual Classification",
            CurriculumSubject::ArabicLanguage => "أنشودة الحروف والألوان ومطابقة الكلمات | Alphabet Chant & Visual Word Matching",
            _ => "اللوحة البصرية والأنشودة التعليمية والتصنيف | Visual Swatches, Chant & Classification",
        },
        "oxford-workbook-print" => match subject {
            CurriculumSubject::IslamicStudies => "تدريبات ومراجعة التربية الإسلامية | Islamic Studies Workbook & Review Activities",
            CurriculumSubject::ArabicLanguage => "تدريبات الإملاء وتكوين الكلمات والخط | Arabic Literacy Workbook & Writing Practice",
            _ => "أنشطة وتدريبات المراجعة التحريرية | Summative Workbook & Practice Exercises",
        },
        "garnet-computing-systems-lab" => match subject {
            CurriculumSubject::IslamicStudies => "خطوات العبادات العملية بالترتيب (الوضوء والصلاة) | Ordered Worship Steps & Practical Flowchart",
            CurriculumSubject::ComputerScience => "مختبر أنظمة الحاسوب وتدفق البيانات (IPO) | Computing Systems Lab & IPO Flowchart",
            _ => "المخطط التسلسلي والخطوات العملية والمقارنة | Step-by-Step Sequence Flowchart & Lab",
        },
        "oxford-phonics-maze" => match subject {
            CurriculumSubject::IslamicStudies => "كتابة المصطلحات الإسلامية ومتاهة العمل الصالح | Islamic Key Terms Writing & Good-Deed Path Maze",
            CurriculumSubject::ArabicLanguage => "تشكيل الحروف الهجائية والخط ومتاهة الحرف | Arabic Letter Formation Staves & Letter Maze",
            _ => "تدريب الكتابة الموجهة ومتاهة المسار الذهني | Guided Writing Lines & Concept Path Maze",
        },
        "cambridge-cpa-grammar" => match subject {
            CurriculumSubject::Mathematics => "التمثيل الحسي والبصري والرمزي (CPA) ومكونات العدد | Concrete-Pictorial-Abstract (CPA) & Number Bonds",
            _ => "القاعدة التطبيقية والأمثلة المتدرجة | Structured Rule Discovery & Guided Examples",
        },
        _ => "صفحة تفاعلية منظمة | Structured Interactive Page",
    }
}

/// Bounded static curriculum outline and spiral page-sequencing helper.
/// Preserves the selected design profile's layout, art, cast, font, and language rules,
/// providing fallback suggestions only when the user request or profile leaves them unspecified.
pub(crate) fn curriculum_blueprint(prompt: &str, skill: &SkillSnapshot, scope: BatchScope, total_pages: u32) -> Result<Value, String> {
    if prompt.trim().is_empty() || prompt.len() > MAX_BRIEF_BYTES {
        return Err(format!("Enter a nonempty brief of at most {MAX_BRIEF_BYTES} UTF-8 bytes (1 MiB)"));
    }
    let profile = profile(skill)?;
    if scope.count == 0 || scope.count > 32 {
        return Err("Invalid native template batch size: must be 1..=32 pages".into());
    }
    let end = scope_end(scope)?;
    if end > 1000 || total_pages == 0 || total_pages > 1000 || total_pages < end {
        return Err("Curriculum page scope exceeds the 1–1000 page document limit".into());
    }
    if let Some(contract) = crate::script_contract::parse(prompt)? {
        return contract.context(scope, total_pages);
    }

    let norm = normalize_curriculum_query(prompt);
    let subject = detect_curriculum_subject(prompt);
    let topic_focus = extract_prompt_topic(prompt);
    let learner_stage = infer_learner_stage(prompt);
    let has_arabic = prompt.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c) && c.is_alphabetic());
    let explicit_bilingual =
        ["ثنائي اللغه", "ثنائيه اللغه", "عربي وانجليزي", "بالعربيه والانجليزيه", "bilingual"].iter().any(|term| norm.contains(term));
    let non_subject_latin_words = norm
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| w.len() >= 2)
        .filter(|w| {
            !matches!(
                *w,
                "math"
                    | "maths"
                    | "mathematics"
                    | "science"
                    | "steam"
                    | "stem"
                    | "computer"
                    | "computing"
                    | "ict"
                    | "coding"
                    | "cpa"
                    | "ipo"
                    | "kg"
                    | "fs"
                    | "eyfs"
                    | "ks"
                    | "igcse"
                    | "ib"
                    | "uk"
                    | "us"
                    | "pirls"
                    | "timss"
                    | "pdf"
                    | "pdfx"
                    | "idml"
                    | "epub"
                    | "json"
                    | "svg"
                    | "png"
                    | "webp"
                    | "jpg"
                    | "jpeg"
                    | "cmyk"
                    | "rgb"
                    | "srgb"
                    | "icc"
                    | "iso"
                    | "fogra"
                    | "coated"
                    | "dpi"
                    | "ppi"
                    | "mm"
                    | "cm"
                    | "pt"
                    | "px"
                    | "rtl"
                    | "ltr"
                    | "ai"
                    | "gemini"
                    | "gpt"
                    | "claude"
                    | "oxford"
                    | "cambridge"
                    | "garnet"
                    | "collins"
                    | "pearson"
                    | "macmillan"
                    | "amiri"
                    | "cairo"
                    | "tajawal"
                    | "readex"
                    | "almarai"
                    | "changa"
                    | "noto"
                    | "naskh"
                    | "kufi"
                    | "ruqaa"
                    | "tahoma"
                    | "arial"
                    | "pro"
                    | "sans"
                    | "serif"
                    | "bold"
                    | "regular"
                    | "medium"
                    | "page"
                    | "pages"
                    | "print"
                    | "trim"
                    | "bleed"
                    | "margin"
                    | "margins"
                    | "cover"
                    | "book"
                    | "islamic"
                    | "islam"
                    | "quran"
                    | "arabic"
                    | "social"
                    | "studies"
                    | "grade"
                    | "level"
            )
        })
        .count();
    let language_mode = if has_arabic && (explicit_bilingual || non_subject_latin_words >= 3) {
        "Bilingual (Arabic & English explicitly requested; follow user brief and profile slot directions)"
    } else if has_arabic {
        "Arabic primary (follow explicit user and profile language; a single Latin subject token such as 'math' does not force bilingual output)"
    } else {
        "English primary (follow explicit user and profile language)"
    };

    let (domain_name, methodology, units, preferred_cycle): (String, &str, Vec<Value>, &[&str]) = match subject {
        CurriculumSubject::IslamicStudies => (
            "Islamic Studies, Noble Quran, Prophetic Seerah & Daily Manners (منهج التربية الإسلامية والقرآن الكريم والسيرة النبوية والآداب)".into(),
            "Age-appropriate spiritual and moral nurturing: Short Surahs & listening reflection, Faith & Tawheed wonder through creation, Ordered Acts of Worship (Purity, Wudu & Prayer steps), Prophetic Seerah stories, and Daily Islamic Manners & Adhkar",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: Allah Our Creator, Blessings & Short Surahs / الوحدة 1: الله خالقي العظيم وتلاوة قصار السور (الفاتحة والإخلاص)",
                    "chapters": [
                        "Ch 1.1: Allah's Beautiful Creation (Sun, Moon, Plants, Family) & Saying Alhamdulillah",
                        "Ch 1.2: Listening and Reciting Surah Al-Fatihah & Surah Al-Ikhlas with Loving Reflection"
                    ],
                    "core_concepts": ["ربّي الله الخالق", "الحمد لله على نعمه", "سورة الفاتحة", "سورة الإخلاص", "أحب القرآن الكريم"],
                    "lab_and_project": "Create a Blessings of Allah gratitude wheel and practice reciting Surah Al-Fatihah with the class"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: Purity, Step-by-Step Wudu & The Joy of Prayer / الوحدة 2: الطهارة والنظافة وخطوات الوضوء والصلاة",
                    "chapters": [
                        "Ch 2.1: Cleanliness in Islam, Saving Water & Ordered Steps of Wudu (الوضوء خطوة بخطوة)",
                        "Ch 2.2: The Call to Prayer (Adhan), Five Daily Prayers & Calm Positions of Salah"
                    ],
                    "core_concepts": ["الطهارة والنظافة", "خطوات الوضوء بالترتيب", "الصلوات الخمس", "القبلة والمسجد", "أحافظ على صلاتي"],
                    "lab_and_project": "Order the illustrated Wudu sequence cards and build a My Daily Prayer Star chart"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: Our Beloved Prophet Muhammad (PBUH) & Moral Stories / الوحدة 3: نبينا محمد ﷺ وقصص الرحمة والصدق والأمانة",
                    "chapters": [
                        "Ch 3.1: Knowing Prophet Muhammad (PBUH), His Kindness to Children & Sending Salawat",
                        "Ch 3.2: 4-Panel Seerah Stories of Truthfulness (Al-Sadiq), Trustworthiness (Al-Amin) & Mercy to Animals"
                    ],
                    "core_concepts": ["نبينا محمد ﷺ", "الصادق الأمين", "الرحمة بالصغار والحيوان", "الصلاة على النبي ﷺ", "الاقتداء بأخلاق الرسول"],
                    "lab_and_project": "Role-play a 4-panel Seerah kindness story and craft a Prophetic Morals classroom tree"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Daily Adhkar, Islamic Manners & Family Kindness / الوحدة 4: الأذكار اليومية والآداب الإسلامية وبر الوالدين",
                    "chapters": [
                        "Ch 4.1: Morning/Evening Adhkar, Greeting with Assalamu Alaikum & Manners of Eating (Bismillah)",
                        "Ch 4.2: Honoring Parents, Sharing with Friends & Summative Islamic Workbook Review"
                    ],
                    "core_concepts": ["بسم الله والحمد لله", "السلام عليكم ورحمة الله", "أذكار الصباح والمساء", "بر الوالدين وإكرام الضيف", "التعاون والرفق"],
                    "lab_and_project": "Make a My Daily Adhkar & Good Deeds flipbook and complete the summative review activities"
                }),
            ],
            &[
                "oxford-vocab-story",
                "oxford-skills-time",
                "oxford-fluency-project",
                "oxford-song-colours",
                "oxford-workbook-print",
                "garnet-computing-systems-lab",
                "oxford-phonics-maze",
                "cambridge-cpa-grammar",
            ],
        ),
        CurriculumSubject::ArabicLanguage => (
            "Arabic Language, Alphabet Literacy, Reading & Expression (منهج اللغة العربية وحروف الهجاء والقراءة والتعبير)".into(),
            "Authentic Arabic literacy progression: Letter shapes (isolated, initial, medial, final), short/long vowels (Harakat & Madd), thematic illustrated vocabulary, 4-panel reading stories, guided Naskh handwriting, and oral fluency",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: Arabic Alphabet Group 1 (أ ب ت ث ج ح خ) & Family Words / الوحدة 1: حروف الهجاء الأولى وعالم الأسرة",
                    "chapters": [
                        "Ch 1.1: Recognizing Letter Sounds, Initial/Medial/Final Shapes & Illustrated Words",
                        "Ch 1.2: Guided Arabic Line Writing, Letter Path Mazes & Short Family Dialogue"
                    ],
                    "core_concepts": ["حروف أ ب ت ث ج ح خ", "أشكال الحرف في الكلمة", "الحركات القصيرة (فتحة ضمة كسرة)", "كلمات الأسرة والبيت"],
                    "lab_and_project": "Trace Arabic letter forms on guided lines and build illustrated letter-word flashcards"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: Nature & School Letters (د ذ ر ز س ش ص ض) & Long Vowels / الوحدة 2: حروف الطبيعة والمدرسة وحروف المد",
                    "chapters": [
                        "Ch 2.1: Connecting Letters with Long Vowels (مد الألف والواو والياء) & Rhythmic Chant",
                        "Ch 2.2: 4-Panel Classroom Story Reading & Demonstrative Sentences (هذا / هذه)"
                    ],
                    "core_concepts": ["حروف د ذ ر ز س ش ص ض", "حروف المد الثلاثة", "هذا وهذه للإشارة", "أناشيد الحروف"],
                    "lab_and_project": "Match syllables to form complete Arabic words and act out the 4-panel school story"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: Reading Comprehension, Listening & Expressive Speaking / الوحدة 3: مهارات القراءة الفاهمة والاستماع والتحدث",
                    "chapters": [
                        "Ch 3.1: Letters (ط ظ ع غ ف ق ك ل) in Short Descriptive Passages & Listening Checks",
                        "Ch 3.2: Constructing Nominal & Verbal Sentences and Pair Question-Answer Practice"
                    ],
                    "core_concepts": ["حروف ط ظ ع غ ف ق ك ل", "اللام الشمسية واللام القمرية", "قراءة جمل قصيرة", "التحدث بلغة عربية سليمة"],
                    "lab_and_project": "Classify Sun and Moon letters in a visual chart and interview a classmate in Arabic"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Completing the Alphabet (م ن هـ و ي), Story Project & Workbook / الوحدة 4: إتمام الحروف والتعبير الكتابي والمشروع",
                    "chapters": [
                        "Ch 4.1: Letters (م ن هـ و ي), Everyday Polite Expressions & Illustrated Story Booklet",
                        "Ch 4.2: Summative Arabic Word Bank, Crossword Puzzle & Guided Sentence Writing"
                    ],
                    "core_concepts": ["حروف م ن هـ و ي", "التاء المربوطة والمفتوحة", "تركيب الكلمات والجمل", "مراجعة الحروف الهجائية"],
                    "lab_and_project": "Assemble an Arabic Alphabet & Stories mini-book and solve the summative workbook review"
                }),
            ],
            &[
                "oxford-vocab-story",
                "oxford-phonics-maze",
                "oxford-skills-time",
                "oxford-song-colours",
                "oxford-fluency-project",
                "oxford-workbook-print",
                "cambridge-cpa-grammar",
                "garnet-computing-systems-lab",
            ],
        ),
        CurriculumSubject::SocialStudies => (
            "Social Studies, Citizenship, Heritage & Geography (منهج الدراسات الاجتماعية والهوية الوطنية والجغرافيا)".into(),
            "Guided social inquiry: Identity and family, community helpers and institutions, maps and natural environments, cultural heritage, and responsible citizenship",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: My Identity, My Family & My Home / الوحدة 1: هويتي وأسرتي وبيتي",
                    "chapters": [
                        "Ch 1.1: Who Am I? Family Members, Roles, Cooperation & Respect at Home",
                        "Ch 1.2: Daily Routines, Timeline of Growth & Safe Home Habits"
                    ],
                    "core_concepts": ["اسمي وهويتي", "أفراد الأسرة والتعاون", "ترتيب الأحداث الزمني", "حقوقي وواجباتي في البيت"],
                    "lab_and_project": "Create an illustrated Family Tree & Home Cooperation chart"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: My School, My Neighborhood & Community Helpers / الوحدة 2: مدرستي وحيّي والمهن في مجتمعي",
                    "chapters": [
                        "Ch 2.1: School Places, Classroom Rules & Helping Each Other",
                        "Ch 2.2: Community Helpers (Teacher, Doctor, Firefighter, Engineer, Farmer) & How Services Work"
                    ],
                    "core_concepts": ["المدرسة والنظام", "أصحاب المهن وخدمة المجتمع", "المؤسسات الخدمية", "احترام العمل"],
                    "lab_and_project": "Role-play community professions and trace how a neighborhood service helps families"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: Places, Maps, Weather & Natural Environments / الوحدة 3: الجغرافيا المبسطة والبيئات الطبيعية والخريطة",
                    "chapters": [
                        "Ch 3.1: Directions, Simple Map Symbols & Landforms (Land, Sea, Mountain, Desert, Oasis)",
                        "Ch 3.2: Weather, Seasons, Water Conservation & Caring for Our Planet"
                    ],
                    "core_concepts": ["الخريطة والاتجاهات", "اليابسة والماء والجبل والصحراء", "فصول السنة والطقس", "المحافظة على البيئة والماء"],
                    "lab_and_project": "Draw a simple classroom/neighborhood map with a symbol key and classify natural environments"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Our Homeland, Cultural Heritage & Good Citizenship / الوحدة 4: وطني وتراثي والمواطنة الصالحة",
                    "chapters": [
                        "Ch 4.1: National Symbols, Traditional Crafts, Hospitality & Historic Landmarks",
                        "Ch 4.2: Traffic Safety, Volunteering & Summative Social Studies Workbook"
                    ],
                    "core_concepts": ["حب الوطن وعلمه", "التراث والعادات الأصيلة", "قواعد السلامة المرورية", "المواطن الصالح"],
                    "lab_and_project": "Build a Heritage & Homeland showcase poster and complete the summative review"
                }),
            ],
            &[
                "oxford-vocab-story",
                "oxford-skills-time",
                "oxford-fluency-project",
                "oxford-song-colours",
                "oxford-workbook-print",
                "garnet-computing-systems-lab",
                "oxford-phonics-maze",
                "cambridge-cpa-grammar",
            ],
        ),
        CurriculumSubject::Mathematics => (
            "Mathematics (Concrete -> Pictorial -> Abstract / CPA & Sensory Numeracy / منهج الرياضيات المتدرج)".into(),
            "Concrete -> Pictorial -> Abstract (CPA) progression, visual subitizing, ten-frames, part-whole number bonds, function machines, and story problem contexts",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: Number Sense, Subitizing & Ten-Frame Place Value / الوحدة 1: الحس العددي وإطار العشرة والقيمة المكانية",
                    "chapters": [
                        "Ch 1.1: Visual Subitizing, Counting Sets & Ten-Frame Representation",
                        "Ch 1.2: Part-Whole Number Bonds & Concrete -> Pictorial -> Abstract (CPA) Equivalence"
                    ],
                    "core_concepts": ["Numbers 1-20", "5-Frame & 10-Frame Subitizing", "Part + Part = Whole Bonds", "Tens & Ones Place Value", "Comparing Quantities (>, <, =)"],
                    "lab_and_project": "Build a tactile Ten-Frame & Part-Whole Number Bond flipboard and verify decompositions"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: Addition, Subtraction & Mental Function Strategies / الوحدة 2: الجمع والطرح وآلة الدوال والحساب الذهني",
                    "chapters": [
                        "Ch 2.1: Combining Sets, Number Line Jumps & Input -> Rule -> Output Function Machines",
                        "Ch 2.2: Real-Life Story Word Problems, Bar Models & Inverse Checking"
                    ],
                    "core_concepts": ["Addition (+)", "Subtraction (-)", "Input -> Rule (+n / -n) -> Output", "Bar Modeling", "Making 10 Strategy"],
                    "lab_and_project": "Construct an Input -> Process (+Rule) -> Output Math Function Machine card and test number inputs"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: 2D/3D Geometry, Spatial Reasoning & Measurement / الوحدة 3: الهندسة والأشكال والقياس المكاني",
                    "chapters": [
                        "Ch 3.1: Properties of 2D Shapes & 3D Solids (Sides, Vertices, Symmetry & Colour Coding)",
                        "Ch 3.2: Measuring Length, Mass, Capacity & Time with Standard Units"
                    ],
                    "core_concepts": ["Circle, Triangle, Square, Hexagon, Cube", "Sides & Corners (Vertices)", "Line of Symmetry", "Length (cm), Mass (g), Time (o'clock)"],
                    "lab_and_project": "Classify and colour-code geometric shapes by attribute and measure classroom objects"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Patterns, Data Handling, Pictograms & Logic Mazes / الوحدة 4: الأنماط وتمثيل البيانات والألغاز المنطقية",
                    "chapters": [
                        "Ch 4.1: Repeating & Growing Number Patterns and Algorithmic Step Mazes",
                        "Ch 4.2: Tally Charts, Pictograms, Bar Graphs & Summative Workbook Review"
                    ],
                    "core_concepts": ["Number & Shape Sequences", "Step-by-Step Math Path Maze", "Tally Marks & Pictograms", "Most / Least / Difference Data Questions"],
                    "lab_and_project": "Conduct a classroom survey, build a Pictogram poster, and complete the Workbook Crossword & Equations"
                }),
            ],
            &[
                "cambridge-cpa-grammar",
                "oxford-vocab-story",
                "oxford-phonics-maze",
                "garnet-computing-systems-lab",
                "oxford-song-colours",
                "oxford-skills-time",
                "oxford-fluency-project",
                "oxford-workbook-print",
            ],
        ),
        CurriculumSubject::ComputerScience => (
            "Computer Science, Coding & ICT Systems (منهج الحاسوب والبرمجة وتقنية المعلومات)".into(),
            "Input -> Process -> Output (IPO) systems architecture, binary data representation, algorithmic sequence mazes, and safe digital citizenship",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: Computer Systems & Hardware Architecture / الوحدة 1: أنظمة الحاسوب والمكونات المادية",
                    "chapters": [
                        "Ch 1.1: Input, Process, Output (IPO) & Peripheral Devices",
                        "Ch 1.2: Inside the System Unit (CPU, Memory, Storage) & Smart Lab Rules"
                    ],
                    "core_concepts": ["Keyboard & Mouse (Input)", "CPU & RAM (Process)", "Monitor & Printer (Output)", "SSD/Disk Storage", "IPO System Flowchart"],
                    "lab_and_project": "Trace a live Input -> Process -> Output task in the computer lab and build a labelled Hardware Poster"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: Data Representation, Binary States & Digital Files / الوحدة 2: تمثيل البيانات والمنطق الثنائي والملفات",
                    "chapters": [
                        "Ch 2.1: Concrete Switches to Binary Digits (0 / 1) & Pixel Grids",
                        "Ch 2.2: Organizing Files, Folders, Tables & Structured Information"
                    ],
                    "core_concepts": ["Bit (0 = Off, 1 = On)", "Byte & Binary Representation", "Pixel Art Grid", "Files, Extensions & Folders"],
                    "lab_and_project": "Encode 4-bit binary patterns on a ten-frame grid and organize a project folder hierarchy"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: Computational Thinking, Algorithms & Visual Block Coding / الوحدة 3: التفكير الحاسوبي والخوارزميات والبرمجة",
                    "chapters": [
                        "Ch 3.1: Step-by-Step Instructions, Sequences & Debugging Path Mazes",
                        "Ch 3.2: Events, Loops (Repeat) & Conditional Logic (If / Then)"
                    ],
                    "core_concepts": ["Algorithm Sequence", "Forward / TurnLeft / TurnRight", "Loop (Repeat N)", "Condition (If path clear)", "Finding & Fixing a Bug"],
                    "lab_and_project": "Guide a robot through an algorithmic grid maze and debug a 4-step block program"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Networks, Safe Digital Citizenship & Creative Media / الوحدة 4: الشبكات والمواطنة الرقمية والمشاريع الإبداعية",
                    "chapters": [
                        "Ch 4.1: How Networks Connect Devices & Online Safety Rules",
                        "Ch 4.2: Collaborative Digital Presentation & Summative Workbook Assessment"
                    ],
                    "core_concepts": ["Local Network & Internet", "Strong Passwords & Privacy", "Digital Kindness", "Multimedia Storyboard"],
                    "lab_and_project": "Create and present a Safe Digital Citizen flipbook and solve the ICT Crossword & Review"
                }),
            ],
            &[
                "garnet-computing-systems-lab",
                "oxford-vocab-story",
                "cambridge-cpa-grammar",
                "oxford-phonics-maze",
                "oxford-skills-time",
                "oxford-fluency-project",
                "oxford-song-colours",
                "oxford-workbook-print",
            ],
        ),
        CurriculumSubject::Science => (
            "Science & Guided Inquiry (منهج العلوم والاستقصاء العلمي)".into(),
            "Scientific inquiry (Observe -> Predict -> Test -> Record), hands-on investigation, and visual concept modeling",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: Living Things, Plants, Animals & Habitats / الوحدة 1: الكائنات الحية والنباتات والبيئات",
                    "chapters": [
                        "Ch 1.1: Parts of a Plant, Photosynthesis Needs & Growth Sequence",
                        "Ch 1.2: Animal Classification, Food Chains & Habitat Adaptation"
                    ],
                    "core_concepts": ["Root, Stem, Leaf, Flower", "Sunlight + Water + Air -> Growth", "Vertebrates & Habitats", "Producer -> Consumer Food Chain"],
                    "lab_and_project": "Observe plant specimens, trace water transport, and build a Habitat Wheel project"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: Materials, States of Matter & Physical Properties / الوحدة 2: المواد وحالات المادة وخصائصها",
                    "chapters": [
                        "Ch 2.1: Solid, Liquid, Gas & Testing Material Properties (Hard, Flexible, Waterproof)",
                        "Ch 2.2: Heating, Cooling, Dissolving & Reversible Changes"
                    ],
                    "core_concepts": ["Solid, Liquid, Gas States", "Rigid vs Flexible, Transparent vs Opaque", "Melting & Freezing", "Fair Testing Lab"],
                    "lab_and_project": "Test 4 classroom materials for strength and water resistance and record findings"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: Forces, Motion, Light & Simple Electrical Circuits / الوحدة 3: القوى والحركة والضوء والدوائر الكهربائية",
                    "chapters": [
                        "Ch 3.1: Push, Pull, Friction, Magnets & Motion Paths",
                        "Ch 3.2: Light Sources, Shadows & Battery -> Switch -> Bulb Circuits"
                    ],
                    "core_concepts": ["Push & Pull Forces", "Magnetic Attraction", "Battery (Input) -> Switch (Control) -> Bulb (Output)", "Light & Shadow Formation"],
                    "lab_and_project": "Build a working simple circuit diagram and test magnetic vs non-magnetic objects"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Earth, Space, Weather & Environmental Stewardship / الوحدة 4: الأرض والفضاء والطقس وحماية البيئة",
                    "chapters": [
                        "Ch 4.1: Day and Night, Seasons & The Water Cycle (Evaporation -> Condensation -> Rain)",
                        "Ch 4.2: Recycling, Clean Energy & Science Fair Project"
                    ],
                    "core_concepts": ["Earth Rotation & Orbit", "Evaporation, Cloud, Precipitation", "Reduce, Reuse, Recycle", "Solar & Wind Energy"],
                    "lab_and_project": "Create a Water Cycle diagram model and complete the Science Workbook Assessment"
                }),
            ],
            &[
                "oxford-vocab-story",
                "cambridge-cpa-grammar",
                "garnet-computing-systems-lab",
                "oxford-song-colours",
                "oxford-skills-time",
                "oxford-fluency-project",
                "oxford-phonics-maze",
                "oxford-workbook-print",
            ],
        ),
        CurriculumSubject::Languages => (
            "Language, Literacy & Communication Curriculum (منهج اللغات والقراءة والتواصل)".into(),
            "Thematic vocabulary, phonics/symbol formation, functional grammar in context, integrated reading/listening/speaking, and project work",
            vec![
                json!({
                    "unit": 1,
                    "title": "Unit 1: Core Vocabulary, Phonics & Classroom Story Context / الوحدة 1: المفردات الأساسية والصوتيات والقصة الصفية",
                    "chapters": [
                        "Ch 1.1: Illustrated Flashcard Vocabulary & 4-Panel Classroom Story Dialogue",
                        "Ch 1.2: Sound-Letter Phonics Arch, 4-Line Guided Writing Staves & Letter Maze"
                    ],
                    "core_concepts": ["Core Thematic Flashcards", "4-Panel Story Comprehension", "Initial & Medial Phonics Sounds", "Sky-Grass-Earth 4-Line Handwriting"],
                    "lab_and_project": "Role-play the 4-panel story dialogue and trace key words on the 4-line staves"
                }),
                json!({
                    "unit": 2,
                    "title": "Unit 2: Grammar in Context (Let's Learn!) & Visual Thinking / الوحدة 2: القواعد الوظيفية والتفكير البصري",
                    "chapters": [
                        "Ch 2.1: Question & Answer Patterns, Contractions & Sentence Building",
                        "Ch 2.2: Visual Swatches, Rhythmic Chant & Attribute Classification"
                    ],
                    "core_concepts": ["What's this? -> It's a ...", "Is it ...? -> Yes, it is / No, it isn't", "Rhythmic Vocabulary Chant", "Colour & Shape Code Matching"],
                    "lab_and_project": "Build grammar puzzle sentences and perform the classroom action chant"
                }),
                json!({
                    "unit": 3,
                    "title": "Unit 3: Skills Time! Integrated Reading, Listening, Speaking & Systems / الوحدة 3: المهارات الأربع المتكاملة والأنظمة",
                    "chapters": [
                        "Ch 3.1: Non-Fiction / Descriptive Reading Passage & Listening Identification (✓)",
                        "Ch 3.2: Structured Pair Speaking Q&A & Process Flowchart"
                    ],
                    "core_concepts": ["Notebook Reading Comprehension", "Listen and Tick ✓ Discrimination", "Ask and Answer Pair Substitution", "Input -> Process -> Output Organization"],
                    "lab_and_project": "Interview a partner using the substitution word pool and present findings"
                }),
                json!({
                    "unit": 4,
                    "title": "Unit 4: Fluency Time! Everyday Communication, Project & Workbook / الوحدة 4: الطلاقة والمشروع العملي وكتاب التمارين",
                    "chapters": [
                        "Ch 4.1: Polite Everyday Classroom Dialogue & 3-Step Hands-On Flipbook Project",
                        "Ch 4.2: Print Workbook Word Bank, Line-Art Numbering, Crossword & Guided Writing"
                    ],
                    "core_concepts": ["Polite Requests & Responses", "Plan -> Build -> Present Project", "I can! 3-Star Self-Assessment", "Summative Crossword & Sentence Writing"],
                    "lab_and_project": "Make and present a thematic flipbook and complete the Workbook review page"
                }),
            ],
            &[
                "oxford-vocab-story",
                "cambridge-cpa-grammar",
                "oxford-phonics-maze",
                "oxford-song-colours",
                "oxford-skills-time",
                "oxford-fluency-project",
                "oxford-workbook-print",
                "garnet-computing-systems-lab",
            ],
        ),
        CurriculumSubject::Storybook
        | CurriculumSubject::Magazine
        | CurriculumSubject::Brochure
        | CurriculumSubject::Report
        | CurriculumSubject::Flashcards
        | CurriculumSubject::ActivityWorkbook
        | CurriculumSubject::PosterOrCertificate
        | CurriculumSubject::Integrated => {
            let genre_label = match subject {
                CurriculumSubject::Storybook => "Illustrated Storybook & Narrative Series (قصة مصورة وسلسلة سردية)",
                CurriculumSubject::Magazine => "Editorial Magazine & Feature Publication (مجلة وإصدار تحريري)",
                CurriculumSubject::Brochure => "Brochure, Catalog & Identity Showcase (بروشور تعريفي وكتالوج)",
                CurriculumSubject::Report => "Structured Report, Study & Data Publication (تقرير ودراسة تحليلية)",
                CurriculumSubject::Flashcards => "Illustrated Flashcards & Visual Learning Deck (بطاقات تعليمية مصورة)",
                CurriculumSubject::ActivityWorkbook => "Interactive Activity & Practice Workbook (كتاب أنشطة وتدريبات تطبيقية)",
                CurriculumSubject::PosterOrCertificate => "Visual Poster, Infographic & Certificate Design (ملصق ولوحة بصرية وشهادة)",
                _ => "Custom Prompt-Driven Publication & Integrated Curriculum (إصدار ومنهج مخصص حسب الطلب)",
            };
            (
                format!("{genre_label} — {topic_focus}"),
                "Dynamic prompt-driven architecture tailored directly to the user's requested topic, genre, language, and visual hierarchy",
                vec![
                    json!({
                        "unit": 1,
                        "title": format!("Unit 1: Introduction & Core Foundations ({topic_focus}) / الوحدة 1: المدخل والمفاهيم الأساسية ({topic_focus})"),
                        "chapters": [
                            format!("Ch 1.1: Visual Overview, Key Pillars & Opening Context ({topic_focus})"),
                            format!("Ch 1.2: Illustrated Concepts & Guided Exploration ({topic_focus})")
                        ],
                        "core_concepts": [&topic_focus, "المفاهيم المحورية", "الهوية البصرية المتناسقة", "التسلسل المنطقي للمحتوى"],
                        "lab_and_project": format!("Explore the foundational themes and visual cards of {topic_focus}")
                    }),
                    json!({
                        "unit": 2,
                        "title": format!("Unit 2: Deep Dive, Stories & Practical Scenarios ({topic_focus}) / الوحدة 2: التفصيل والقصص والمواقف التطبيقية"),
                        "chapters": [
                            format!("Ch 2.1: Real-World Scenarios, Illustrated Panels & Case Examples ({topic_focus})"),
                            format!("Ch 2.2: Visual Classification, Comparisons & Key Highlights ({topic_focus})")
                        ],
                        "core_concepts": ["أمثلة واقعية مصورة", "مقارنة وتصنيف", "ترابط الفقرات والبطاقات"],
                        "lab_and_project": format!("Analyze real-world examples and visual comparisons for {topic_focus}")
                    }),
                    json!({
                        "unit": 3,
                        "title": format!("Unit 3: Step-by-Step Workflows, Reading & Analysis ({topic_focus}) / الوحدة 3: الخطوات المنظمة والتحليل التطبيقي"),
                        "chapters": [
                            format!("Ch 3.1: Detailed Reading, Key Takeaways & Comprehension Check ({topic_focus})"),
                            format!("Ch 3.2: Ordered Process Flowchart, Structured Table & Guided Discussion ({topic_focus})")
                        ],
                        "core_concepts": ["خطوات عملية متسلسلة", "جداول ومخططات توضيحية", "أسئلة فهم واستيعاب"],
                        "lab_and_project": format!("Follow the step-by-step workflow and structured summary for {topic_focus}")
                    }),
                    json!({
                        "unit": 4,
                        "title": format!("Unit 4: Creative Project, Hands-On Application & Review ({topic_focus}) / الوحدة 4: المشروع التطبيقي والمراجعة الشاملة"),
                        "chapters": [
                            format!("Ch 4.1: 3-Step Practical Project & Self-Check ({topic_focus})"),
                            format!("Ch 4.2: Comprehensive Summary, Interactive Exercises & Closing Synthesis ({topic_focus})")
                        ],
                        "core_concepts": ["مشروع تطبيقي عملي", "تقييم ذاتي ومراجعة", "خلاصة شاملة"],
                        "lab_and_project": format!("Complete the hands-on project and comprehensive review for {topic_focus}")
                    }),
                ],
                &[
                    "oxford-vocab-story",
                    "oxford-skills-time",
                    "oxford-fluency-project",
                    "oxford-song-colours",
                    "oxford-workbook-print",
                    "garnet-computing-systems-lab",
                    "oxford-phonics-maze",
                    "cambridge-cpa-grammar",
                ],
            )
        }
    };

    // A suggested family must be accepted by the same strict compiler. Fixed
    // counter artwork remains unavailable until its quantities are bound safely.
    let declared_families: Vec<&str> = profile
        .page_families
        .iter()
        .filter(|family| {
            skill.templates.get(&family.template).is_some_and(|template| {
                !counting_family(family, template) || (subject == CurriculumSubject::Mathematics && bindable_counting_family(family, template))
            })
        })
        .map(|family| family.id.as_str())
        .collect();
    if declared_families.is_empty() {
        return Err("No compilable native page families remain; unbound fixed counting artwork requires structured adaptive activities".into());
    }
    let has_family = |id: &str| declared_families.contains(&id);
    let cover_family = ["international-curriculum-cover", "cover"].into_iter().find(|id| has_family(id));
    let scope_family = ["international-scope-sequence", "curriculum-map"].into_iter().find(|id| has_family(id));
    let opener_family = declared_families.iter().copied().find(|id| *id == "observe-growth-sequence");

    let mut body_cycle: Vec<&str> = preferred_cycle.iter().copied().filter(|id| has_family(id) && Some(*id) != opener_family).collect();
    for fam in &declared_families {
        if Some(*fam) != cover_family && Some(*fam) != scope_family && Some(*fam) != opener_family && !body_cycle.contains(fam) {
            body_cycle.push(fam);
        }
    }
    if body_cycle.is_empty() {
        body_cycle.extend(declared_families.iter().copied().filter(|family| Some(*family) != opener_family));
    }

    let eff_total = total_pages.max(end).max(1);
    let use_cover = eff_total >= 2 && cover_family.is_some();
    let use_scope = eff_total >= 4 && scope_family.is_some();
    let use_opener = !use_cover && opener_family.is_some();
    let body_start_page = u32::from(use_cover).saturating_add(u32::from(use_scope)).saturating_add(u32::from(use_opener));
    let body_pages_total = eff_total.saturating_sub(body_start_page).max(1);
    if body_cycle.is_empty() && eff_total > body_start_page {
        return Err("The selected profile has no compilable body family after its page-one opener".into());
    }

    let mut scoped_page_plan = Vec::with_capacity(usize::try_from(scope.count).unwrap_or(0));
    for page in scope.start..end {
        let recommended_family = if page == 0 && use_cover {
            cover_family.unwrap_or("cover")
        } else if page == 0 && use_opener {
            opener_family.ok_or("The native page-one opener disappeared")?
        } else if page == 1 && use_scope {
            scope_family.unwrap_or("curriculum-map")
        } else if !body_cycle.is_empty() {
            let offset = usize::try_from(page.saturating_sub(body_start_page)).unwrap_or(0);
            let idx = offset.checked_rem(body_cycle.len()).unwrap_or(0);
            body_cycle.get(idx).copied().or_else(|| declared_families.first().copied()).unwrap_or("practice")
        } else {
            declared_families.first().copied().unwrap_or("practice")
        };

        let (unit_num, chapter_label, cognitive_stage, prev_link, intra_flow, next_bridge) = if page == 0 && use_cover {
            (
                1usize,
                "Book Opener & Original Series Cover / غلاف الكتاب وهوية السلسلة الأصلية".to_owned(),
                "Stage 1: Original Series Title, Course Overview Pillars & Student Card",
                "Entry point of the book: introduces the user's original series identity and motivates the learner without claiming external publisher affiliation.",
                "Follow the cover family's declared text and image slots -> Course Title & Subtitle -> Hero illustration -> 4 Overview Pillars -> Student registration box.",
                "Prepares the learner and educator for the 4-Unit Scope & Sequence syllabus map on the next page.",
            )
        } else if page == 1 && use_scope {
            (
                1usize,
                "Suggested Scope & Sequence Map (Units 1–4) / خريطة تسلسل الوحدات المقترحة".to_owned(),
                "Stage 2: Suggested Syllabus Outline (Units, Vocabulary, Core Logic & Hands-On Projects)",
                "Builds on the cover's overview pillars by mapping them into 4 progressive units for educator review.",
                "Follow the syllabus family's declared slots -> Panoramic banner -> Column headers -> 4 structured unit rows (Unit 1 to Unit 4).",
                "Launches Unit 1 Lesson 1 on the next page with its core concepts and guided activities.",
            )
        } else {
            let body_idx = page.saturating_sub(body_start_page);
            let scaled = u64::from(body_idx).saturating_mul(4) / u64::from(body_pages_total);
            let unit_idx = usize::try_from(scaled.min(3)).unwrap_or(0);
            let unit_no = unit_idx.saturating_add(1);
            let lesson_no = (body_idx % 4).saturating_add(1);
            let unit_title = units.get(unit_idx).and_then(|u| u.get("title")).and_then(Value::as_str).unwrap_or("Core Unit");
            let stage = match (subject, recommended_family) {
                (CurriculumSubject::IslamicStudies, "oxford-vocab-story") => "Islamic Concept Cards & 4-Panel Prophetic Seerah / Daily Manners Story",
                (CurriculumSubject::IslamicStudies, "oxford-skills-time") => {
                    "Short Surah / Hadith Reflection, Listening Check & Guided Pair Discussion"
                }
                (CurriculumSubject::IslamicStudies, "oxford-fluency-project") => {
                    "Daily Adhkar & Sunnah Dialogue, 3-Step Good Deeds Craft & Self-Check"
                }
                (CurriculumSubject::IslamicStudies, "garnet-computing-systems-lab") => {
                    "Step-by-Step Worship Sequence (Wudu / Prayer / Moral Action Flowchart) & Practical Application"
                }
                (CurriculumSubject::IslamicStudies, "oxford-phonics-maze") => "Key Islamic Term Writing Staves & Good-Deed Path Maze",
                (_, "cambridge-cpa-grammar") => "Concrete -> Pictorial -> Abstract (CPA) Rule Discovery & Part-Whole Reasoning",
                (_, "oxford-vocab-story") => "Contextual Concept Vocabulary Flashcards & 4-Panel Story/Scenario Dialogue",
                (_, "oxford-phonics-maze") => "Symbol/Number/Term Formation Staves & Algorithmic Sequence Maze",
                (_, "garnet-computing-systems-lab") => "Input -> Process/Rule -> Output Systems Flowchart & Practical Lab Investigation",
                (_, "oxford-song-colours") => "Visual Swatches, Panoramic Exploration Chant & Code Classification",
                (_, "oxford-skills-time") => "Integrated Reading Passage, Listening Discrimination & Pair Talk",
                (_, "oxford-fluency-project") => "Collaborative Dialogue, 3-Step Hands-On Project & Self-Check",
                (_, "oxford-workbook-print") => "Summative Print Workbook: Word/Number Bank, Line-Art, Crossword & Writing",
                _ => "Guided Observation, Structured Practice & Independent Application",
            };
            (
                unit_no,
                format!("{unit_title} — Lesson {lesson_no}"),
                stage,
                "Recalls and applies the vocabulary, symbols or concepts introduced on the immediately preceding page.",
                "Follow the selected page family's native composition and declared slot purposes, connecting opening prompts, central visual/analytical frames, and concluding student tasks.",
                "Consolidates the lesson objective and bridges into the next page's deeper application or review.",
            )
        };

        let display_archetype = family_display_archetype(subject, recommended_family);
        scoped_page_plan.push(json!({
            "page": page,
            "recommended_family": recommended_family,
            "display_archetype": display_archetype,
            "unit_number": unit_num,
            "chapter_and_lesson": chapter_label,
            "cognitive_stage": cognitive_stage,
            "mental_chain": {
                "prerequisite_from_previous_page": prev_link,
                "intra_page_flow": intra_flow,
                "spiral_bridge_to_next_page": next_bridge
            }
        }));
    }

    let family_slot_max_chars: BTreeMap<&str, BTreeMap<&str, usize>> = profile
        .page_families
        .iter()
        .filter(|family| declared_families.contains(&family.id.as_str()))
        .map(|family| {
            let slots = family.text_slots.iter().map(|slot| (slot.id.as_str(), slot.max_chars)).collect();
            (family.id.as_str(), slots)
        })
        .collect();

    Ok(json!({
        "outline_status": "Suggested static domain outline (not live external research or standards retrieval); educator review and verification are required.",
        "series_identity_policy": "Use the user's original series identity and title; do not reproduce publisher ribbons/logos or claim publisher affiliation, endorsement, certification, or measured similarity.",
        "family_slot_max_chars": family_slot_max_chars,
        "selected_profile_authority": {
            "style_id": skill.skill_id,
            "name": profile.name,
            "age_range": profile.age_range,
            "inferred_learner_stage": learner_stage,
            "page_mm": profile.page_mm,
            "margin_mm": profile.margin_mm,
            "palette": profile.palette,
            "typography": profile.typography,
            "layout": profile.layout,
            "pedagogy": profile.pedagogy,
            "illustration": profile.illustration,
            "precedence_rule": "The explicit user request and this selected design_profile retain full control over art backgrounds, recurring characters, fonts, age range, language, and page compositions. Use fallback suggestions below only when left unspecified by the request and profile."
        },
        "subject_domain": domain_name,
        "prompt_topic_focus": topic_focus,
        "learner_stage": learner_stage,
        "language_and_script_mode": language_mode,
        "suggested_methodology": methodology,
        "units_and_chapters": units,
        "scoped_page_plan": scoped_page_plan,
        "fallback_defaults_when_unspecified": {
            "dialogue_cast": {
                "precedence": "Preserve any recurring characters specified by the user request or selected design_profile/skill guidance (such as the original recurring pair in science editorial styles).",
                "fallback_students": ["Omar (عمر)", "Noor (نور)", "Lina (لينا)", "Karim (كريم)"],
                "fallback_teacher": "Teacher Sarah (المعلمة سارة)",
                "dialogue_rule": "Keep speech bubbles natural, encouraging, and directly tied to the page's concept."
            },
            "illustration_guidance": {
                "precedence": "Follow the selected design_profile's illustration rules and background palette first (e.g. warm cream backgrounds when specified by the profile, or clean cutout backgrounds when specified by flashcard families).",
                "raster_text_ban": "Strictly NO text, NO letters, NO numbers, and NO speech bubbles inside generated raster images."
            },
            "typography_and_font_rules": {
                "precedence": "Preserve the authored template font family, point size, and leading from the selected design_profile; glyph coverage is verified by actual native preflight.",
                "direction_and_shaping": "Logical Unicode order, preserving requested or foundational early-years Arabic tashkeel/harakat marks, automatic rightToLeft for Arabic slots and leftToRight for English/equation slots. Aim for each slot's max_chars base-character copy target (combining harakat marks do not count against max_chars); native composition verifies actual fit without deleting supplied wording or shrinking authored type."
            }
        }
    }))
}

fn strip_arabic_marks_for_match(input: &str) -> String {
    input
        .chars()
        .filter(|&c| {
            !(('\u{0610}'..='\u{061A}').contains(&c)
                || ('\u{064B}'..='\u{065F}').contains(&c)
                || c == '\u{0670}'
                || ('\u{06D6}'..='\u{06ED}').contains(&c)
                || c == '\u{0640}')
        })
        .map(|c| match c {
            'أ' | 'إ' | 'آ' | 'ٱ' => 'ا',
            _ => c,
        })
        .collect()
}

pub(super) fn parse_small_unit_int(token: &str) -> Option<u32> {
    let mut digits = String::new();
    for ch in token.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else if ('\u{0660}'..='\u{0669}').contains(&ch) {
            digits.push(char::from_u32(u32::from('0') + (u32::from(ch) - 0x0660))?);
        } else if ('\u{06F0}'..='\u{06F9}').contains(&ch) {
            digits.push(char::from_u32(u32::from('0') + (u32::from(ch) - 0x06F0))?);
        } else if !digits.is_empty() {
            break;
        }
    }
    digits.parse::<u32>().ok().filter(|&n| (1..=99).contains(&n))
}

pub(super) fn extract_unit_number_from_text(raw: &str) -> Option<u32> {
    let cleaned = strip_arabic_marks_for_match(raw);
    let lower = cleaned.to_lowercase();
    for (phrase, num) in [
        ("الوحدة الثانية عشرة", 12),
        ("الوحدة الحادية عشرة", 11),
        ("الوحدة العاشرة", 10),
        ("الوحدة التاسعة", 9),
        ("الوحدة الثامنة", 8),
        ("الوحدة السابعة", 7),
        ("الوحدة السادسة", 6),
        ("الوحدة الخامسة", 5),
        ("الوحدة الرابعة", 4),
        ("الوحدة الثالثة", 3),
        ("الوحدة الثانية", 2),
        ("الوحدة الاولى", 1),
        ("الوحدة الأولى", 1),
    ] {
        if lower.contains(phrase) {
            return Some(num);
        }
    }
    for marker in ["الوحدة", "وحدة", "unit"] {
        if let Some(pos) = lower.find(marker)
            && let Some(after) = lower.get(pos.saturating_add(marker.len())..)
        {
            let rest = after.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '-' | '–' | '—' | '#' | '(' | '['));
            if let Some(n) = parse_small_unit_int(rest) {
                return Some(n);
            }
        }
    }
    None
}
pub(super) fn seed_unit_from_preceding_pages(base: &designcraft_engine::DocState, upto_page: u32) -> Option<u32> {
    let doc = &base.doc;
    let limit = usize::try_from(upto_page).unwrap_or(0);
    if limit == 0 {
        return None;
    }
    let mut active: Option<u32> = None;
    for sec in &doc.sections {
        if sec.start < limit
            && let Some(n) = parse_small_unit_int(sec.marker.trim())
        {
            active = Some(n);
        }
    }
    for (spread_idx, spread) in doc.spreads.iter().enumerate() {
        let first_page = doc.first_page_of_spread(spread_idx);
        let mut stack: Vec<(&designcraft_engine::doc::Item, designcraft_geom::Affine)> =
            spread.items.iter().rev().map(|it| (it.as_ref(), designcraft_geom::Affine::IDENTITY)).collect();
        while let Some((item, parent_xf)) = stack.pop() {
            let bounds = parent_xf.transform_rect_bbox(item.bounds());
            let page_offset = spread.page_at_x(bounds.center().x).unwrap_or(0);
            let page_idx = first_page.saturating_add(page_offset);
            if page_idx < limit
                && let Some(tf) = item.text_frame()
                && let Some(story) = doc.stories.get(&tf.story)
            {
                if item.label == "part.ui.header.unit-num" {
                    if let Some(n) = parse_small_unit_int(story.text.trim()) {
                        active = Some(n);
                    }
                } else if let Some(n) = extract_unit_number_from_text(&story.text) {
                    active = Some(n);
                }
            }
            if !item.children().is_empty() {
                let child_xf = parent_xf * item.xf;
                for child in item.children().iter().rev() {
                    stack.push((child.as_ref(), child_xf));
                }
            }
        }
    }
    active
}
