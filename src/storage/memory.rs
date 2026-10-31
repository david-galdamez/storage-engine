use std::collections::HashMap;

#[derive(Debug)]
pub struct Storage {
    data: HashMap<String, String>,
}

impl Storage {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.data.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: &str, value: &str) -> Option<String> {
        self.data.insert(key.to_string(), value.to_string())
    }

    pub fn delete(&mut self, key: &str) -> Option<String> {
        self.data.remove(key)
    }

    pub fn scan(&self) -> impl Iterator<Item = (&str, &str)> {
        self.data.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}
