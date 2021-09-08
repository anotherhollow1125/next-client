use crate::meta::NCInfo;
use crate::*;
use anyhow::{Error, Result};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use std::time::Duration;
use tokio::sync::mpsc;

pub enum NetworkStatus {
    Connect,
    Disconnect,
    Err(Error),
}

impl std::cmp::PartialEq for NetworkStatus {
    fn eq(&self, other: &Self) -> bool {
        match self {
            &Self::Connect => match other {
                &Self::Connect => true,
                _ => false,
            },
            _ => match other {
                &Self::Connect => false,
                _ => true,
            },
        }
    }
}

impl std::cmp::Eq for NetworkStatus {}

pub async fn status_raw(nc_info: &NCInfo, client: &reqwest::Client) -> NetworkStatus {
    // let res = reqwest::get(&nc_info.host).await;
    let res = client
        .get(&nc_info.host)
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    match res {
        Ok(_) => NetworkStatus::Connect,
        Err(e) if e.is_connect() => NetworkStatus::Disconnect,
        Err(e) => {
            error!("{:?}", e);
            NetworkStatus::Err(Error::new(e))
        }
    }
}

pub async fn is_online(nc_info: &NCInfo, client: &reqwest::Client) -> bool {
    match status_raw(nc_info, client).await {
        NetworkStatus::Connect => true,
        _ => false,
    }
}

pub async fn status(nc_info: &NCInfo, client: &reqwest::Client) -> Result<NetworkStatus> {
    // let res = reqwest::get(&nc_info.host).await;
    let res = client
        .get(&nc_info.host)
        .timeout(Duration::from_secs(5))
        .send()
        .await;

    match res {
        Ok(_) => Ok(NetworkStatus::Connect),
        Err(e) if e.is_connect() => Ok(NetworkStatus::Disconnect),
        Err(e) => {
            error!("{:?}", e);
            Err(e.into())
        }
    }
}

pub async fn check(
    tx: &mpsc::Sender<Command>,
    nc_info: &NCInfo,
    client: &reqwest::Client,
) -> Result<bool> {
    let res = is_online(nc_info, client).await;

    if res {
        tx.send(Command::NetworkConnect).await?;
    } else {
        tx.send(Command::NetworkDisconnect).await?;
    }

    Ok(res)
}
