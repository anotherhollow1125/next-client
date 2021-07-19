use dotenv::dotenv;
use ncs::nc_listen::*;
use ncs::*;
use std::env;
#[allow(unused)]
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    let username = env::var("NC_USERNAME").expect("NC_USERNAME not found");
    let password = env::var("NC_PASSWORD").expect("NC_PASSWORD not found");
    let host = env::var("NC_HOST").expect("NC_HOST not found");
    let root_path = env::var("NC_ROOT").expect("NC_ROOT not found");

    let host = fix_host(&host);
    let root_path = fix_root(&root_path);

    let nc_info = NCInfo::new(username, password, host, root_path);

    let local_root_path = env::var("LOCAL_ROOT").expect("LOCAL_ROOT not found");
    let local_info = LocalInfo::new(local_root_path);

    let mut args = env::args();
    let target = args.nth(1).unwrap_or("".to_string());

    let (target_entry, mut book) = from_nc_all(&nc_info, &target).await?;

    println!("{}", target_entry.borrow().get_tree());
    // println!("{:?}", book);

    let paths = book
        .values()
        .map(|e| e.borrow().path.clone())
        .collect::<Vec<_>>();

    for p in paths.into_iter() {
        make_entry(&p, &nc_info, &local_info, &mut book).await?;
    }

    Ok(())
}
