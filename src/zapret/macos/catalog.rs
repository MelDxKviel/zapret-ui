use crate::contracts::{Category, Strategy};
use crate::ports::StrategyCatalog;
use crate::zapret::macos_bundle::valid_strategy_id;
use std::path::PathBuf;

pub struct LocalStrategyCatalog {
    install_dir: PathBuf,
}
impl LocalStrategyCatalog {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }
}
impl StrategyCatalog for LocalStrategyCatalog {
    fn all(&self) -> Vec<Strategy> {
        let text =
            std::fs::read_to_string(self.install_dir.join("strategies.tsv")).unwrap_or_default();
        let mut seen = std::collections::HashSet::new();
        text.lines()
            .filter_map(|line| {
                let (id, name) = line.split_once('\t')?;
                if !valid_strategy_id(id)
                    || name.trim().is_empty()
                    || !seen.insert(id)
                    || !self
                        .install_dir
                        .join("strategies")
                        .join(format!("{id}.conf.in"))
                        .is_file()
                {
                    return None;
                }
                Some(Strategy {
                    id: id.into(),
                    display_name: name.trim().into(),
                    category: Category::Mixed,
                    description: String::new(),
                    winws_args: Vec::new(),
                    requires_lists: Vec::new(),
                })
            })
            .collect()
    }
    fn by_id(&self, id: &str) -> Option<Strategy> {
        self.all().into_iter().find(|s| s.id == id)
    }
    fn by_category(&self, category: Category) -> Vec<Strategy> {
        self.all()
            .into_iter()
            .filter(|s| s.category == category)
            .collect()
    }
}
