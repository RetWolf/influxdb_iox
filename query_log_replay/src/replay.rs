use std::path::Path;

use structopt::StructOpt;

use influxdb_iox_client::connection::Connection;

use crate::query_log::QueryLog;

pub type Result<T, E = String> = std::result::Result<T, E>;

/// Replay the contents of previously saved queries from a file back to a databse
#[derive(Debug, StructOpt)]
pub struct Replay {
    /// The database name to replay the queries against
    db: String,

    /// The filename to replay the queries to
    filename: String,
}

impl Replay {
    pub async fn execute(&self, connection: Connection) -> Result<()> {
        println!(
            "Replaying from {} into database {}...",
            self.db, self.filename
        );
        let path = Path::new(&self.filename);

        let log = QueryLog::new_from_file(path).await?;

        println!("Loaded query log with {} entries", log.queries.len());
        //println!("Loaded log:\n\n{:#?}", log);

        // now execute the queries against the specified database and connection
        for query in log.queries.into_iter().map(|r| r.into_inner()) {
            let description = query.to_string();
            let execution = query.replay(&self.db, connection.clone()).await?;
            println!("Ran {}: {}", description, execution);
        }

        Ok(())
    }
}
