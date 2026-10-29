use crate::schema::{DataType, Value};
use std::fmt;

// ───────────────────────────── Token ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords
    Select,
    From,
    Where,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Create,
    Table,
    Drop,
    Begin,
    Commit,
    Rollback,
    And,
    Or,
    Not,
    Null,
    Primary,
    Key,
    Unique,
    Default,
    Autoincrement,
    If,
    Exists,
    Check,
    References,
    Foreign,
    Constraint,

    // Literals
    Integer(i64),
    Real(f64),
    StringLiteral(String),

    // Identifiers
    Ident(String),

    // Operators & punctuation
    Star,      // *
    Comma,     // ,
    LParen,    // (
    RParen,    // )
    Semicolon, // ;
    Eq,        // =
    Neq,       // != or <>
    Lt,        // <
    Gt,        // >
    LtEq,      // <=
    GtEq,      // >=

    Eof,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

// ───────────────────────────── Tokenizer ─────────────────────────

pub struct Tokenizer {
    chars: Vec<char>,
    pos: usize,
}

impl Tokenizer {
    pub fn new(input: &str) -> Self {
        Tokenizer {
            chars: input.chars().collect(),
            pos: 0,
        }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace();
            if self.pos >= self.chars.len() {
                tokens.push(Token::Eof);
                break;
            }
            let ch = self.chars[self.pos];
            match ch {
                '*' => {
                    tokens.push(Token::Star);
                    self.pos += 1;
                }
                ',' => {
                    tokens.push(Token::Comma);
                    self.pos += 1;
                }
                '(' => {
                    tokens.push(Token::LParen);
                    self.pos += 1;
                }
                ')' => {
                    tokens.push(Token::RParen);
                    self.pos += 1;
                }
                ';' => {
                    tokens.push(Token::Semicolon);
                    self.pos += 1;
                }
                '=' => {
                    tokens.push(Token::Eq);
                    self.pos += 1;
                }
                '!' => {
                    self.pos += 1;
                    if self.pos < self.chars.len() && self.chars[self.pos] == '=' {
                        tokens.push(Token::Neq);
                        self.pos += 1;
                    } else {
                        return Err("unexpected '!'".to_string());
                    }
                }
                '<' => {
                    self.pos += 1;
                    if self.pos < self.chars.len() && self.chars[self.pos] == '=' {
                        tokens.push(Token::LtEq);
                        self.pos += 1;
                    } else if self.pos < self.chars.len() && self.chars[self.pos] == '>' {
                        tokens.push(Token::Neq);
                        self.pos += 1;
                    } else {
                        tokens.push(Token::Lt);
                    }
                }
                '>' => {
                    self.pos += 1;
                    if self.pos < self.chars.len() && self.chars[self.pos] == '=' {
                        tokens.push(Token::GtEq);
                        self.pos += 1;
                    } else {
                        tokens.push(Token::Gt);
                    }
                }
                '\'' => {
                    tokens.push(self.read_string()?);
                }
                c if c.is_ascii_digit() || (c == '-' && self.peek_digit()) => {
                    tokens.push(self.read_number()?);
                }
                c if c.is_ascii_alphabetic() || c == '_' => {
                    tokens.push(self.read_ident_or_keyword());
                }
                _ => {
                    return Err(format!("unexpected character: '{}'", ch));
                }
            }
        }
        Ok(tokens)
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek_digit(&self) -> bool {
        self.pos + 1 < self.chars.len() && self.chars[self.pos + 1].is_ascii_digit()
    }

    fn read_string(&mut self) -> Result<Token, String> {
        self.pos += 1; // skip opening quote
        let mut s = String::new();
        while self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            if ch == '\'' {
                // Check for escaped quote ('')
                if self.pos + 1 < self.chars.len() && self.chars[self.pos + 1] == '\'' {
                    s.push('\'');
                    self.pos += 2;
                } else {
                    self.pos += 1;
                    return Ok(Token::StringLiteral(s));
                }
            } else {
                s.push(ch);
                self.pos += 1;
            }
        }
        Err("unterminated string literal".to_string())
    }

    fn read_number(&mut self) -> Result<Token, String> {
        let start = self.pos;
        if self.chars[self.pos] == '-' {
            self.pos += 1;
        }
        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        let mut is_real = false;
        if self.pos < self.chars.len() && self.chars[self.pos] == '.' {
            is_real = true;
            self.pos += 1;
            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        if is_real {
            s.parse::<f64>()
                .map(Token::Real)
                .map_err(|e| format!("invalid real: {}", e))
        } else {
            s.parse::<i64>()
                .map(Token::Integer)
                .map_err(|e| format!("invalid integer: {}", e))
        }
    }

    fn read_ident_or_keyword(&mut self) -> Token {
        let start = self.pos;
        while self.pos < self.chars.len()
            && (self.chars[self.pos].is_ascii_alphanumeric() || self.chars[self.pos] == '_')
        {
            self.pos += 1;
        }
        let word: String = self.chars[start..self.pos].iter().collect();
        match word.to_uppercase().as_str() {
            "SELECT" => Token::Select,
            "FROM" => Token::From,
            "WHERE" => Token::Where,
            "INSERT" => Token::Insert,
            "INTO" => Token::Into,
            "VALUES" => Token::Values,
            "UPDATE" => Token::Update,
            "SET" => Token::Set,
            "DELETE" => Token::Delete,
            "CREATE" => Token::Create,
            "TABLE" => Token::Table,
            "DROP" => Token::Drop,
            "BEGIN" => Token::Begin,
            "COMMIT" => Token::Commit,
            "ROLLBACK" => Token::Rollback,
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            "NULL" => Token::Null,
            "PRIMARY" => Token::Primary,
            "KEY" => Token::Key,
            "UNIQUE" => Token::Unique,
            "DEFAULT" => Token::Default,
            "AUTOINCREMENT" | "AUTO_INCREMENT" => Token::Autoincrement,
            "IF" => Token::If,
            "EXISTS" => Token::Exists,
            "CHECK" => Token::Check,
            "REFERENCES" => Token::References,
            "FOREIGN" => Token::Foreign,
            "CONSTRAINT" => Token::Constraint,
            _ => Token::Ident(word),
        }
    }
}

// ───────────────────────────── AST ─────────────────────────────

#[derive(Debug, Clone)]
pub enum Statement {
    CreateTable {
        table_name: String,
        columns: Vec<ColumnDef>,
    },
    DropTable {
        table_name: String,
    },
    Insert {
        table_name: String,
        columns: Option<Vec<String>>,
        values: Vec<Vec<Expr>>,
    },
    Select {
        columns: Vec<SelectColumn>,
        table_name: String,
        where_clause: Option<Expr>,
    },
    Update {
        table_name: String,
        assignments: Vec<(String, Expr)>,
        where_clause: Option<Expr>,
    },
    Delete {
        table_name: String,
        where_clause: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub is_primary_key: bool,
    pub is_not_null: bool,
    pub is_unique: bool,
    pub is_autoincrement: bool,
    /// Declared exactly `INTEGER PRIMARY KEY`: SQLite's rowid alias.
    pub is_rowid_alias: bool,
    pub default_value: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum SelectColumn {
    AllColumns,
    Named(String),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Value),
    ColumnRef(String),
    BinaryOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Eq,
    Neq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
}

// ───────────────────────────── Parser ─────────────────────────────

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    pub fn parse(input: &str) -> Result<Statement, String> {
        let mut tokenizer = Tokenizer::new(input);
        let tokens = tokenizer.tokenize()?;
        let mut parser = Parser::new(tokens);
        parser.parse_statement()
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), String> {
        let tok = self.advance();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(())
        } else {
            Err(format!("expected {:?}, got {:?}", expected, tok))
        }
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.advance() {
            Token::Ident(s) => Ok(s),
            other => Err(format!("expected identifier, got {:?}", other)),
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, String> {
        match self.peek().clone() {
            Token::Create => self.parse_create(),
            Token::Drop => self.parse_drop(),
            Token::Insert => self.parse_insert(),
            Token::Select => self.parse_select(),
            Token::Update => self.parse_update(),
            Token::Delete => self.parse_delete(),
            Token::Begin => {
                self.advance();
                self.skip_semicolon();
                Ok(Statement::Begin)
            }
            Token::Commit => {
                self.advance();
                self.skip_semicolon();
                Ok(Statement::Commit)
            }
            Token::Rollback => {
                self.advance();
                self.skip_semicolon();
                Ok(Statement::Rollback)
            }
            _ => Err(format!("unexpected token: {:?}", self.peek())),
        }
    }

    fn skip_semicolon(&mut self) {
        if self.peek() == &Token::Semicolon {
            self.advance();
        }
    }

    fn parse_create(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Create)?;
        self.expect(&Token::Table)?;
        let table_name = self.expect_ident()?;
        self.expect(&Token::LParen)?;

        let mut columns = Vec::new();
        loop {
            // Handle trailing comma: if next token is RParen, stop.
            if self.peek() == &Token::RParen {
                break;
            }
            let col = self.parse_column_def()?;
            columns.push(col);
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        self.skip_semicolon();
        Ok(Statement::CreateTable {
            table_name,
            columns,
        })
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef, String> {
        let name = self.expect_ident()?;
        // Read type name — could be a keyword like NOT reused as type, or an ident.
        let type_name = match self.peek().clone() {
            Token::Ident(s) => {
                self.advance();
                s
            }
            // Some type names collide with keywords (e.g., BLOB, INTEGER parsed as idents)
            // but also accept keywords that might be type names.
            Token::Integer(_) | Token::Real(_) => {
                // No type specified, treat as TEXT.
                return Ok(ColumnDef {
                    name,
                    data_type: DataType::Text,
                    is_primary_key: false,
                    is_not_null: false,
                    is_unique: false,
                    is_autoincrement: false,
                    is_rowid_alias: false,
                    default_value: None,
                });
            }
            _ => {
                // Try to use the token text as a type name
                // For things like BLOB being parsed as Ident, this handles it.
                // If it's RParen or Comma, no type was given — default to TEXT.
                if matches!(self.peek(), Token::Comma | Token::RParen) {
                    return Ok(ColumnDef {
                        name,
                        data_type: DataType::Text,
                        is_primary_key: false,
                        is_not_null: false,
                        is_unique: false,
                        is_autoincrement: false,
                        is_rowid_alias: false,
                        default_value: None,
                    });
                }
                return Err(format!("expected type name, got {:?}", self.peek()));
            }
        };
        let data_type =
            DataType::from_str(&type_name).ok_or_else(|| format!("unknown type: {}", type_name))?;

        // SQLite only treats a PRIMARY KEY as the rowid alias when the declared type
        // is exactly "INTEGER" (any case, no size arguments).
        let mut type_is_integer = type_name.eq_ignore_ascii_case("INTEGER");

        // Handle parameterized types like VARCHAR(50), DECIMAL(10,2), CHAR(1)
        if self.peek() == &Token::LParen {
            type_is_integer = false;
            self.advance(); // skip '('
            // Consume everything until matching ')'
            let mut depth = 1;
            while depth > 0 {
                match self.advance() {
                    Token::LParen => depth += 1,
                    Token::RParen => depth -= 1,
                    Token::Eof => return Err("unterminated type parameter".to_string()),
                    _ => {} // skip numbers, commas, etc.
                }
            }
        }

        let mut is_primary_key = false;
        let mut is_not_null = false;
        let mut is_unique = false;
        let mut is_autoincrement = false;
        let mut default_value = None;

        // Parse column constraints (loop to handle multiple constraints).
        loop {
            match self.peek().clone() {
                Token::Primary => {
                    self.advance();
                    self.expect(&Token::Key)?;
                    is_primary_key = true;
                }
                Token::Not => {
                    self.advance();
                    self.expect(&Token::Null)?;
                    is_not_null = true;
                }
                Token::Unique => {
                    self.advance();
                    is_unique = true;
                }
                Token::Autoincrement => {
                    self.advance();
                    is_autoincrement = true;
                }
                Token::Default => {
                    self.advance();
                    // Parse the default value expression.
                    default_value = Some(self.parse_primary()?);
                }
                Token::Check => {
                    // Skip CHECK(...) constraints.
                    self.advance();
                    if self.peek() == &Token::LParen {
                        self.advance();
                        let mut depth = 1;
                        while depth > 0 {
                            match self.advance() {
                                Token::LParen => depth += 1,
                                Token::RParen => depth -= 1,
                                Token::Eof => return Err("unterminated CHECK".to_string()),
                                _ => {}
                            }
                        }
                    }
                }
                Token::References => {
                    // Skip REFERENCES table(col) foreign key inline.
                    self.advance();
                    let _ = self.expect_ident(); // table name
                    if self.peek() == &Token::LParen {
                        self.advance();
                        let mut depth = 1;
                        while depth > 0 {
                            match self.advance() {
                                Token::LParen => depth += 1,
                                Token::RParen => depth -= 1,
                                Token::Eof => return Err("unterminated REFERENCES".to_string()),
                                _ => {}
                            }
                        }
                    }
                }
                _ => break,
            }
        }

        Ok(ColumnDef {
            name,
            data_type,
            is_primary_key,
            is_not_null,
            is_unique,
            is_autoincrement,
            is_rowid_alias: is_primary_key && type_is_integer,
            default_value,
        })
    }

    fn parse_drop(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Drop)?;
        self.expect(&Token::Table)?;
        let table_name = self.expect_ident()?;
        self.skip_semicolon();
        Ok(Statement::DropTable { table_name })
    }

    fn parse_insert(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Insert)?;
        self.expect(&Token::Into)?;
        let table_name = self.expect_ident()?;

        // Optional column list.
        let columns = if self.peek() == &Token::LParen {
            self.advance();
            let mut cols = Vec::new();
            loop {
                cols.push(self.expect_ident()?);
                if self.peek() == &Token::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };

        self.expect(&Token::Values)?;

        let mut all_values = Vec::new();
        loop {
            self.expect(&Token::LParen)?;
            let mut vals = Vec::new();
            loop {
                vals.push(self.parse_expr()?);
                if self.peek() == &Token::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&Token::RParen)?;
            all_values.push(vals);
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }

        self.skip_semicolon();
        Ok(Statement::Insert {
            table_name,
            columns,
            values: all_values,
        })
    }

    fn parse_select(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Select)?;

        let mut columns = Vec::new();
        if self.peek() == &Token::Star {
            self.advance();
            columns.push(SelectColumn::AllColumns);
        } else {
            loop {
                let name = self.expect_ident()?;
                columns.push(SelectColumn::Named(name));
                if self.peek() == &Token::Comma {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        self.expect(&Token::From)?;
        let table_name = self.expect_ident()?;

        let where_clause = if self.peek() == &Token::Where {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.skip_semicolon();
        Ok(Statement::Select {
            columns,
            table_name,
            where_clause,
        })
    }

    fn parse_update(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Update)?;
        let table_name = self.expect_ident()?;
        self.expect(&Token::Set)?;

        let mut assignments = Vec::new();
        loop {
            let col = self.expect_ident()?;
            self.expect(&Token::Eq)?;
            let val = self.parse_expr()?;
            assignments.push((col, val));
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }

        let where_clause = if self.peek() == &Token::Where {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.skip_semicolon();
        Ok(Statement::Update {
            table_name,
            assignments,
            where_clause,
        })
    }

    fn parse_delete(&mut self) -> Result<Statement, String> {
        self.expect(&Token::Delete)?;
        self.expect(&Token::From)?;
        let table_name = self.expect_ident()?;

        let where_clause = if self.peek() == &Token::Where {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.skip_semicolon();
        Ok(Statement::Delete {
            table_name,
            where_clause,
        })
    }

    // ─── Expression parsing (simple precedence: OR < AND < comparison) ───

    fn parse_expr(&mut self) -> Result<Expr, String> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_and_expr()?;
        while self.peek() == &Token::Or {
            self.advance();
            let right = self.parse_and_expr()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::Or,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, String> {
        let mut left = self.parse_comparison()?;
        while self.peek() == &Token::And {
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::And,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr, String> {
        let left = self.parse_primary()?;
        let op = match self.peek() {
            Token::Eq => Some(BinOp::Eq),
            Token::Neq => Some(BinOp::Neq),
            Token::Lt => Some(BinOp::Lt),
            Token::Gt => Some(BinOp::Gt),
            Token::LtEq => Some(BinOp::LtEq),
            Token::GtEq => Some(BinOp::GtEq),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let right = self.parse_primary()?;
            Ok(Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
        } else {
            Ok(left)
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::Integer(i) => {
                self.advance();
                Ok(Expr::Literal(Value::Integer(i)))
            }
            Token::Real(r) => {
                self.advance();
                Ok(Expr::Literal(Value::Real(r)))
            }
            Token::StringLiteral(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s)))
            }
            Token::Null => {
                self.advance();
                Ok(Expr::Literal(Value::Null))
            }
            Token::Ident(name) => {
                self.advance();
                Ok(Expr::ColumnRef(name))
            }
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            other => Err(format!("unexpected token in expression: {:?}", other)),
        }
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenizer_basic() {
        let mut t = Tokenizer::new("SELECT * FROM users WHERE id = 1;");
        let tokens = t.tokenize().unwrap();
        assert!(matches!(tokens[0], Token::Select));
        assert!(matches!(tokens[1], Token::Star));
        assert!(matches!(tokens[2], Token::From));
        assert!(matches!(tokens[3], Token::Ident(_)));
        assert!(matches!(tokens[4], Token::Where));
        assert!(matches!(tokens[5], Token::Ident(_)));
        assert!(matches!(tokens[6], Token::Eq));
        assert!(matches!(tokens[7], Token::Integer(1)));
        assert!(matches!(tokens[8], Token::Semicolon));
    }

    #[test]
    fn test_tokenizer_string() {
        let mut t = Tokenizer::new("INSERT INTO t VALUES ('hello world');");
        let tokens = t.tokenize().unwrap();
        assert!(matches!(tokens[4], Token::LParen));
        assert!(matches!(tokens[5], Token::StringLiteral(_)));
        if let Token::StringLiteral(s) = &tokens[5] {
            assert_eq!(s, "hello world");
        }
    }

    #[test]
    fn test_parser_create() {
        let stmt =
            Parser::parse("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);")
                .unwrap();
        match stmt {
            Statement::CreateTable {
                table_name,
                columns,
            } => {
                assert_eq!(table_name, "users");
                assert_eq!(columns.len(), 3);
                assert!(columns[0].is_primary_key);
                assert!(columns[0].is_rowid_alias);
                assert!(!columns[2].is_rowid_alias);
                assert_eq!(columns[1].data_type, DataType::Text);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn test_parser_rowid_alias_needs_exact_integer() {
        let sql = "CREATE TABLE t (a integer primary key, b INT PRIMARY KEY, \
                   c INTEGER(10) PRIMARY KEY, d INTEGER UNIQUE);";
        match Parser::parse(sql).unwrap() {
            Statement::CreateTable { columns, .. } => {
                let alias: Vec<bool> = columns.iter().map(|c| c.is_rowid_alias).collect();
                assert_eq!(alias, vec![true, false, false, false]);
            }
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn test_parser_insert() {
        let stmt =
            Parser::parse("INSERT INTO users (id, name) VALUES (1, 'Alice'), (2, 'Bob');").unwrap();
        match stmt {
            Statement::Insert {
                table_name,
                columns,
                values,
            } => {
                assert_eq!(table_name, "users");
                assert_eq!(columns.unwrap().len(), 2);
                assert_eq!(values.len(), 2);
            }
            _ => panic!("expected Insert"),
        }
    }

    #[test]
    fn test_parser_select() {
        let stmt = Parser::parse("SELECT name, age FROM users WHERE age > 18;").unwrap();
        match stmt {
            Statement::Select {
                columns,
                table_name,
                where_clause,
            } => {
                assert_eq!(table_name, "users");
                assert_eq!(columns.len(), 2);
                assert!(where_clause.is_some());
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn test_parser_update() {
        let stmt = Parser::parse("UPDATE users SET name = 'Charlie' WHERE id = 1;").unwrap();
        match stmt {
            Statement::Update {
                table_name,
                assignments,
                where_clause,
            } => {
                assert_eq!(table_name, "users");
                assert_eq!(assignments.len(), 1);
                assert!(where_clause.is_some());
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn test_parser_delete() {
        let stmt = Parser::parse("DELETE FROM users WHERE id = 2;").unwrap();
        match stmt {
            Statement::Delete {
                table_name,
                where_clause,
            } => {
                assert_eq!(table_name, "users");
                assert!(where_clause.is_some());
            }
            _ => panic!("expected Delete"),
        }
    }

    #[test]
    fn test_parser_transactions() {
        assert!(matches!(Parser::parse("BEGIN;").unwrap(), Statement::Begin));
        assert!(matches!(
            Parser::parse("COMMIT;").unwrap(),
            Statement::Commit
        ));
        assert!(matches!(
            Parser::parse("ROLLBACK;").unwrap(),
            Statement::Rollback
        ));
    }

    #[test]
    fn test_parser_select_all() {
        let stmt = Parser::parse("SELECT * FROM products;").unwrap();
        match stmt {
            Statement::Select {
                columns,
                table_name,
                ..
            } => {
                assert_eq!(table_name, "products");
                assert!(matches!(columns[0], SelectColumn::AllColumns));
            }
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn test_parser_where_and() {
        let stmt = Parser::parse("SELECT * FROM t WHERE a = 1 AND b = 2;").unwrap();
        match stmt {
            Statement::Select { where_clause, .. } => {
                let wc = where_clause.unwrap();
                assert!(matches!(wc, Expr::BinaryOp { op: BinOp::And, .. }));
            }
            _ => panic!("expected Select"),
        }
    }
}
