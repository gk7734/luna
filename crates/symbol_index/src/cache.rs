use anyhow::Result;
use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};

/// Each indexed file's symbols and TODOs keyed by absolute path, with the modification time and
/// size they were read at, so reopening a project only parses files that changed since.
pub struct SymbolIndexDb(ThreadSafeConnection);

impl Domain for SymbolIndexDb {
    const NAME: &str = stringify!(SymbolIndexDb);
    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS symbol_index_files(
            path TEXT NOT NULL PRIMARY KEY,
            mtime INTEGER NOT NULL,
            size INTEGER NOT NULL,
            contents BLOB NOT NULL
        ) STRICT;
    )];
}

db::static_connection!(SymbolIndexDb, []);

pub struct CachedFileRow {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    pub contents: Vec<u8>,
}

impl SymbolIndexDb {
    query! {
        pub fn cached_file(path: String) -> Result<Option<(i64, i64, Vec<u8>)>> {
            SELECT mtime, size, contents FROM symbol_index_files WHERE path = (?)
        }
    }

    pub async fn save_files(&self, rows: Vec<CachedFileRow>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.write(move |connection| {
            connection.with_savepoint("save_symbol_index_files", || {
                let mut insert = connection.exec_bound::<(String, i64, i64, Vec<u8>)>(sql!(
                    INSERT OR REPLACE INTO symbol_index_files(path, mtime, size, contents)
                    VALUES (?, ?, ?, ?)
                ))?;
                for row in rows {
                    insert((row.path, row.mtime, row.size, row.contents))?;
                }
                Ok(())
            })
        })
        .await
    }

    pub async fn delete_files(&self, paths: Vec<String>) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.write(move |connection| {
            connection.with_savepoint("delete_symbol_index_files", || {
                let mut delete = connection
                    .exec_bound::<String>(sql!(DELETE FROM symbol_index_files WHERE path = (?)))?;
                for path in paths {
                    delete(path)?;
                }
                Ok(())
            })
        })
        .await
    }
}
