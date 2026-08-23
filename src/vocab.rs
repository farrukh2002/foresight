use std::collections::HashMap;

#[derive(Default)]
pub struct Vocab {
    pub strings: Vec<String>,
    ids: HashMap<String, u32>,
    pub(crate) sorted_ids: Vec<u32>,
}

impl Vocab {
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.ids.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(s.to_string());
        self.ids.insert(s.to_string(), id);
        let pos = self
            .sorted_ids
            .partition_point(|&i| self.strings[i as usize].as_str() < s);
        self.sorted_ids.insert(pos, id);
        id
    }

    pub fn get(&self, s: &str) -> Option<u32> {
        self.ids.get(s).copied()
    }

    pub fn text(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }

    pub fn rebuild_sorted(&mut self) {
        let mut ids: Vec<u32> = (0..self.strings.len() as u32).collect();
        ids.sort_by(|&a, &b| self.strings[a as usize].cmp(&self.strings[b as usize]));
        self.sorted_ids = ids;
    }

    pub fn prefix_range(&self, prefix: &str) -> (usize, usize) {
        let start = self.sorted_ids.partition_point(|&i| self.strings[i as usize].as_str() < prefix);
        let mut end = start;
        while end < self.sorted_ids.len() && self.strings[self.sorted_ids[end] as usize].starts_with(prefix) {
            end += 1;
        }
        (start, end)
    }
}
