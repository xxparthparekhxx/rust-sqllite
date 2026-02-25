use rust_sqllite::Database;
use std::io::{self, BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let db_path = if args.len() > 1 {
        args[1].clone()
    } else {
        "rustsqlite.db".to_string()
    };

    let mut db = match Database::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error: could not open database '{}': {}", db_path, e);
            std::process::exit(1);
        }
    };

    println!("RustSQLite v1.0 — A SQLite implementation in pure Rust");
    println!("Connected to: {}", db_path);
    println!("Type .help for usage hints. Type .quit to exit.");
    println!();

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut input_buf = String::new();

    loop {
        // Prompt.
        if input_buf.is_empty() {
            print!("rustsqlite> ");
        } else {
            print!("       ...> ");
        }
        stdout.flush().unwrap();

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                // EOF (Ctrl+D / Ctrl+Z).
                println!();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error reading input: {}", e);
                break;
            }
        }

        let trimmed = line.trim();

        // Skip empty lines.
        if trimmed.is_empty() {
            continue;
        }

        // Dot-commands (meta commands like psql's \commands).
        if input_buf.is_empty() && trimmed.starts_with('.') {
            if handle_dot_command(trimmed, &mut db) {
                break; // .quit requested
            }
            continue;
        }

        input_buf.push_str(&line);

        // Check if the statement is complete (ends with semicolon).
        let full = input_buf.trim();
        if !full.ends_with(';') {
            continue;
        }

        // Execute the accumulated input.
        let sql = input_buf.trim().to_string();
        input_buf.clear();

        execute_and_print(&mut db, &sql);
    }

    println!("Bye!");
    if let Err(e) = db.close() {
        eprintln!("Warning: error closing database: {}", e);
    }
}

fn handle_dot_command(cmd: &str, db: &mut Database) -> bool {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    match parts[0] {
        ".quit" | ".exit" | ".q" => {
            return true; // signal main loop to break, allowing db.close()
        }
        ".help" | ".h" => {
            println!("Meta-commands:");
            println!("  .help          Show this help");
            println!("  .quit          Exit the shell");
            println!("  .tables        List all tables");
            println!("  .schema [TBL]  Show CREATE statement for table(s)");
            println!("  .checkpoint    Force a WAL checkpoint");
            println!("  .demo          Run a built-in demo with sample data");
            println!();
            println!("SQL commands end with a semicolon (;):");
            println!("  CREATE TABLE, INSERT, SELECT, UPDATE, DELETE");
            println!("  BEGIN, COMMIT, ROLLBACK");
        }
        ".tables" | ".table" => {
            let tables = db.list_tables();
            if tables.is_empty() {
                println!("(no tables)");
            } else {
                for t in &tables {
                    println!("  {}", t);
                }
            }
        }
        ".schema" => {
            println!("(schema introspection coming soon)");
        }
        ".checkpoint" | ".cp" => match db.checkpoint() {
            Ok(_) => println!("WAL checkpoint completed."),
            Err(e) => eprintln!("Checkpoint error: {}", e),
        },
        ".demo" => {
            run_demo(db);
        }
        _ => {
            eprintln!("Unknown command: {}. Try .help", parts[0]);
        }
    }
    false
}

fn execute_and_print(db: &mut Database, sql: &str) {
    match db.execute(sql) {
        Ok(result) => {
            if !result.rows.is_empty() {
                print_table(&result.columns, &result.rows);
                println!(
                    "({} row{})",
                    result.rows.len(),
                    if result.rows.len() == 1 { "" } else { "s" }
                );
            } else if result.rows_affected > 0 {
                println!(
                    "OK ({} row{} affected)",
                    result.rows_affected,
                    if result.rows_affected == 1 { "" } else { "s" }
                );
            } else {
                println!("OK");
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
        }
    }
}

fn print_table(columns: &[String], rows: &[Vec<rust_sqllite::Value>]) {
    if rows.is_empty() {
        return;
    }

    let col_names: Vec<String> = if columns.is_empty() {
        (0..rows[0].len()).map(|i| format!("col{}", i)).collect()
    } else {
        columns.to_vec()
    };

    // Compute column widths.
    let mut widths: Vec<usize> = col_names.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, val) in row.iter().enumerate() {
            let s = format!("{}", val);
            if i < widths.len() && s.len() > widths[i] {
                widths[i] = s.len();
            }
        }
    }

    // Header.
    let header: String = col_names
        .iter()
        .enumerate()
        .map(|(i, c)| format!(" {:<w$} ", c, w = widths[i]))
        .collect::<Vec<_>>()
        .join("│");
    println!("{}", header);

    // Separator.
    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(w + 2))
        .collect::<Vec<_>>()
        .join("┼");
    println!("{}", sep);

    // Data rows.
    for row in rows {
        let line: String = row
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let s = format!("{}", v);
                let w = if i < widths.len() { widths[i] } else { s.len() };
                format!(" {:<w$} ", s, w = w)
            })
            .collect::<Vec<_>>()
            .join("│");
        println!("{}", line);
    }
}

fn run_demo(db: &mut Database) {
    println!("── Running demo ──");
    let commands = [
        "CREATE TABLE users (id INTEGER, name TEXT, email TEXT, age INTEGER);",
        "INSERT INTO users VALUES (1, 'Alice', 'alice@example.com', 28);",
        "INSERT INTO users VALUES (2, 'Bob', 'bob@example.com', 35);",
        "INSERT INTO users VALUES (3, 'Charlie', 'charlie@example.com', 22);",
        "INSERT INTO users VALUES (4, 'Diana', 'diana@example.com', 30);",
        "SELECT * FROM users;",
        "SELECT name, age FROM users WHERE age > 25;",
        "UPDATE users SET age = 29 WHERE name = 'Alice';",
        "SELECT * FROM users WHERE name = 'Alice';",
        "DELETE FROM users WHERE age < 25;",
        "SELECT * FROM users;",
    ];

    for sql in &commands {
        println!("rustsqlite> {}", sql);
        execute_and_print(db, sql);
        println!();
    }
    println!("── Demo complete ──");
}
