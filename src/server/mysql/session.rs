use std::collections::HashMap;

use crate::db::SqlColumn;

#[derive(Debug, Clone)]
pub(super) struct PreparedStatement {
    pub bound_sql: String,
    pub param_count: usize,
    pub param_types: Vec<u8>,
    pub columns: Vec<SqlColumn>,
}

#[derive(Debug)]
pub(super) struct MySqlSession {
    pub username: String,
    pub database: String,
    pub _capabilities: u32,
    pub _charset: u8,
    pub _attrs: HashMap<String, String>,
    pub statements: HashMap<u32, PreparedStatement>,
    pub next_statement_id: u32,
}

impl MySqlSession {
    pub fn new(
        username: String,
        database: Option<String>,
        capabilities: u32,
        attrs: HashMap<String, String>,
    ) -> Self {
        Self {
            username,
            database: database.unwrap_or_else(|| "memory".to_string()),
            _capabilities: capabilities,
            _charset: 45,
            _attrs: attrs,
            statements: HashMap::new(),
            next_statement_id: 1,
        }
    }

    pub fn next_statement_id(&mut self) -> u32 {
        let id = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1).max(1);
        id
    }
}
