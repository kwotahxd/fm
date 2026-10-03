use fm_types::{Rule, RuleSet};

fn single(name: &str, description: &str, dest: &str) -> RuleSet {
    RuleSet {
        name: name.into(),
        description: description.into(),
        attributes: vec![],
        rules: vec![Rule { name: name.into(), dest: dest.into(), ..Default::default() }],
    }
}

/// Rule sets that work without any AI.
pub fn all() -> Vec<RuleSet> {
    vec![
        single("by-type", "Images / Video / Audio / Documents / Code / Archives / Binaries / Other", "{kind}"),
        single("by-date", "year/month from the modification time (UTC)", "{year}/{month}"),
        single("by-ext", "one folder per file extension", "{ext}"),
        single("by-category", "one folder per AI category (needs `fm analyze` first)", "{category}"),
    ]
}

pub fn get(name: &str) -> Option<RuleSet> {
    all().into_iter().find(|r| r.name == name)
}
