use dotenv::dotenv;
use ncs::nc_listen::*;
use ncs::*;
use std::collections::HashMap;
use std::env;
#[allow(unused)]
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    env_logger::init();
    let username = env::var("NC_USERNAME").expect("NC_USERNAME not found");
    let password = env::var("NC_PASSWORD").expect("NC_PASSWORD not found");
    let host = env::var("NC_HOST").expect("NC_HOST not found");
    // let root_path = env::var("NC_ROOT").expect("NC_ROOT not found");

    let host = fix_host(&host);
    // let root_path = fix_root(&root_path);

    let nc_info = NCInfo::new(username, password, host);

    let local_root_path = env::var("LOCAL_ROOT").expect("LOCAL_ROOT not found");
    let local_info = LocalInfo::new(local_root_path);

    let mut args = env::args();
    match args.nth(1) {
        Some(s) if s == "init" => {
            let _ = init(&nc_info, &local_info).await?;
        }
        Some(s) if s == "update" => {
            if let Some(v) = args.nth(0) {
                let mut nc_state = NCState {
                    latest_activity_id: v,
                };
                let events = get_ncevents(&nc_info, &mut nc_state).await?;
                println!("{}", nc_state.latest_activity_id);
                for e in events {
                    println!("{:?}", e);
                }
            }
        }
        _ => (),
    }

    Ok(())
}

async fn init(nc_info: &NCInfo, local_info: &LocalInfo) -> anyhow::Result<HashMap<String, Entry>> {
    let latest_activity_id = get_latest_activity_id(&nc_info).await?;
    println!("{}", latest_activity_id);

    let (root_path, mut book) = from_nc_all(&nc_info, "").await?;

    println!(
        "{}",
        book.get(&root_path)
            .map(|p| p.get_tree(&book))
            .unwrap_or("Error!".to_string())
    );

    init_local_entries(&nc_info, &local_info, &mut book).await?;

    println!(
        "{}",
        book.get(&root_path)
            .map(|p| p.get_tree(&book))
            .unwrap_or("Error!".to_string())
    );

    Ok(book)
}
