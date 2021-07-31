use crate::errors::NcsError::*;
use crate::*;
use anyhow::{Context, Result};
use log::{debug, info};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{Client, Method, Url};
use std::io;
use std::sync::{Arc, Mutex};
use urlencoding::decode;

const NC_ROOT_PREFIX: &str = "/remote.php/dav/files/";
pub const OCS_ROOT: &str = "/ocs/v2.php/apps/activity/api/v2/activity/all";

pub const WEBDAV_BODY: &str = r#"<?xml version="1.0"?>
<d:propfind  xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
  <d:prop>
        <d:getetag />
        <d:getcontenttype />
  </d:prop>
</d:propfind>
"#;

pub struct NCInfo {
    pub username: String,
    pub password: String,
    pub host: String,
    pub root_path: String,
}

impl NCInfo {
    pub fn new(username: String, password: String, host: String) -> Self {
        let host = fix_host(&host);
        let root_path = format!("{}{}", NC_ROOT_PREFIX, username);
        let root_path = fix_root(&root_path);
        Self {
            username,
            password,
            host,
            root_path,
        }
    }
}

pub struct NCState {
    pub latest_activity_id: String,
}

pub async fn from_nc(nc_info: &NCInfo, target: &str) -> Result<Entry> {
    let target = add_head_slash(&target);
    let responses = comm_nc(nc_info, &target).await?;

    let target_name = path2name(&target);
    let target_name_without_slash = drop_slash(&target_name, &RE_HAS_LAST_SLASH);

    debug!("target_name: {}", target_name);

    let target_res = responses
        .into_iter()
        .filter(|r| {
            debug!("r_name: {}", &r.get_name());
            r.get_raw_name().as_str() == &target_name_without_slash
        })
        .nth(0);

    target_res.with_context(|| format!("Can not find target Entry."))
}

pub async fn ncpath_is_file(nc_info: &NCInfo, target: &str) -> bool {
    let entry_res = from_nc(nc_info, target).await;

    if let Ok(entry) = entry_res {
        entry.type_.is_file()
    } else {
        false
    }
}

/*
pub async fn correct_nc_path(nc_info: &NCInfo, target: &str) -> String {
    let is_file = ncpath_is_file(nc_info, target).await;
    if is_file {
        drop_slash(target, &RE_HAS_LAST_SLASH)
    } else {
        let res = add_last_slash(target);
        debug!("{}", res);
        res
    }
}
*/

pub async fn get_etag_from_nc(nc_info: &NCInfo, target: &str) -> Option<String> {
    let entry_res = from_nc(nc_info, target).await;

    if_chain! {
        if let Ok(entry) = entry_res;
        if let EntryType::File { etag } = entry.type_;
        then {
            etag
        } else {
            None
        }
    }
}

pub async fn from_nc_all(nc_info: &NCInfo, target: &str) -> Result<ArcEntry> {
    let target = add_head_slash(&target);
    let top_entry = from_nc(nc_info, &target).await?;
    let top_entry = Arc::new(Mutex::new(top_entry));

    get_children_rec(nc_info, &top_entry, "").await?;

    Ok(top_entry)
}

#[async_recursion]
async fn get_children_rec(
    nc_info: &NCInfo,
    parent_entry: &ArcEntry,
    ancestor_path: &str,
) -> Result<()> {
    let parent_name = parent_entry.lock().map_err(|_| LockError)?.get_name();
    let ancestor_path = format!("{}{}", ancestor_path, parent_name);
    let children_entries = comm_nc(nc_info, &ancestor_path)
        .await?
        .into_iter()
        .filter(|c| c.get_name().as_str() != &parent_name);

    for c in children_entries {
        let c = Arc::new(Mutex::new(c));
        get_children_rec(nc_info, &c, &ancestor_path).await?;
        Entry::append_child(parent_entry, c)?;
    }

    Ok(())
}

async fn comm_nc(nc_info: &NCInfo, target: &str) -> Result<Vec<Entry>> {
    let target = add_head_slash(target);
    let target = drop_slash(&target, &RE_HAS_LAST_SLASH);

    let path = format!("{}{}", &nc_info.root_path, target)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();

    let mut url = Url::parse(&nc_info.host)?;
    url.path_segments_mut().unwrap().extend(path);

    let res = Client::new()
        .request(Method::from_bytes(b"PROPFIND").unwrap(), url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("Depth", "Infinity")
        .body(WEBDAV_BODY)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(BadStatusError(res.status().as_u16()).into());
    }

    let text = res.text_with_charset("utf-8").await?;

    // debug!("{}", text);

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = webdav_xml2responses(&document, &nc_info.root_path);

    Ok(responses)
}

fn webdav_xml2responses(document: &roxmltree::Document, root_path: &str) -> Vec<Entry> {
    document
        .root_element()
        .children()
        .map(|n| {
            if n.tag_name().name() != "response" {
                return None;
            }

            let mut name_w = None;
            let mut etag_w = None;
            let mut type_w = None;

            for m in n.children() {
                match m.tag_name().name() {
                    "href" => {
                        if let Some(href) = m.text() {
                            let path = href.replace(&root_path, "");
                            let path = decode(&path).ok()?;
                            name_w = Some(path2name(&path));
                        }
                    }
                    "propstat" => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "getetag" => {
                                    etag_w = d.text().and_then(|s| Some(s.replace("\"", "")));
                                }
                                "getcontenttype" => {
                                    type_w = match d.text() {
                                        Some(ref s) if s != &"" => {
                                            Some(EntryType::File { etag: None })
                                        }
                                        _ => Some(EntryType::Directory),
                                    };
                                }
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
            }
            if_chain! {
                if let Some(name) = name_w;
                if let Some(etag) = etag_w;
                if let Some(type_) = type_w;
                then {
                    let type_ = if let EntryType::File {..} = type_ {
                        EntryType::File { etag: Some(etag) }
                    } else {
                        type_
                    };

                    Some(Entry::new(name, type_))
                } else {
                    None
                }
            }
        })
        .filter_map(|v| v)
        .collect()
}

fn save_file<R: io::Read>(r: &mut R, path: &str, local_info: &LocalInfo) -> Result<()> {
    let filename = format!("{}{}", local_info.root_path, path);
    fileope::save_file(r, &filename)?;

    Ok(())
}

fn create_dir_all(path: &str, local_info: &LocalInfo) -> Result<()> {
    let dirname = format!("{}{}", local_info.root_path, path);
    fileope::create_dir_all(&dirname)?;

    Ok(())
}

fn touch_entry(path: &str, is_file: bool, local_info: &LocalInfo) -> Result<()> {
    let path = format!("{}{}", local_info.root_path, path);
    fileope::touch_entry(path, is_file)?;

    Ok(())
}

fn move_entry(from_path: &str, to_path: &str, stash: bool, local_info: &LocalInfo) -> Result<()> {
    let from_path = format!("{}{}", local_info.root_path, from_path);
    let to_path = format!("{}{}", local_info.root_path, to_path);
    fileope::move_entry(from_path, to_path, stash)?;

    Ok(())
}

fn remove_entry(path: &str, stash: bool, local_info: &LocalInfo) -> Result<()> {
    let path = format!("{}{}", local_info.root_path, path);
    fileope::remove_entry(path, stash)?;

    Ok(())
}

#[async_recursion(?Send)]
pub async fn init_local_entries(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &ArcEntry,
    ancestor_path: &str,
) -> Result<()> {
    let mut entry_ref = entry.lock().map_err(|_| LockError)?;

    if entry_ref.status == EntryStatus::UpToDate {
        return Ok(());
    }

    let full_path = format!("{}{}", ancestor_path, entry_ref.get_name());
    match &entry_ref.type_ {
        &EntryType::Directory => {
            create_dir_all(&full_path, local_info)?;
            entry_ref.status = EntryStatus::UpToDate;
            for c in entry_ref.children.values() {
                init_local_entries(nc_info, local_info, c, &full_path).await?;
            }
        }
        &EntryType::File { .. } => {
            create_dir_all(ancestor_path, local_info)?;
            let res =
                download_file_in_rec(nc_info, local_info, &mut entry_ref, ancestor_path).await;
            match res {
                Ok(()) => {
                    entry_ref.status = EntryStatus::UpToDate;
                }
                Err(e) => {
                    entry_ref.status = EntryStatus::Error;
                    return Err(e);
                }
            }
        }
    }

    Ok(())
}

async fn download_file_raw(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &mut Entry,
    full_path: &str,
) -> Result<()> {
    if entry.type_.is_dir() {
        return Err(anyhow!("Not file entry!!"));
    }

    let mut url = Url::parse(&nc_info.host)?;
    let path_v = format!("{}{}", nc_info.root_path, full_path)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let data_res = Client::new()
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .send()
        .await?;

    let new_etag = data_res
        .headers()
        .get("ETag")
        .with_context(|| format!("Can't get new etag."))
        .and_then(|v| v.to_str().with_context(|| "Can't get new etag."))
        .map(|v| v.to_string().replace("\"", ""))?;
    entry.type_ = EntryType::File {
        etag: Some(new_etag),
    };

    let bytes = data_res.bytes().await?;
    save_file(&mut bytes.as_ref(), full_path, local_info)?;

    Ok(())
}

async fn download_file_in_rec(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &mut Entry,
    ancestor_path: &str,
) -> Result<()> {
    let full_path = format!("{}{}", ancestor_path, entry.get_name());

    download_file_raw(nc_info, local_info, entry, &full_path).await
}

pub async fn download_file_with_check_etag(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    entry: &ArcEntry,
) -> Result<()> {
    let full_path = Entry::get_path(entry)?;

    let nc_etag = get_etag_from_nc(nc_info, &full_path).await;

    {
        let mut entry_ref = entry.lock().map_err(|_| LockError)?;
        if_chain! {
            if let Some(etag) = nc_etag;
            if entry_ref.type_ != EntryType::File { etag: Some(etag) };
            then {
                debug!("Need to download.");
                download_file_raw(nc_info, local_info, &mut entry_ref, &full_path).await?;
            }
        }
    }

    Ok(())
}

pub async fn get_latest_activity_id(nc_info: &NCInfo) -> Result<String> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = OCS_ROOT
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let res = Client::new()
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("OCS-APIRequest", "true")
        .send()
        .await?;

    res.headers()
        .get("X-Activity-First-Known")
        .with_context(|| format!("Can't get latest activity id."))
        .and_then(|v| v.to_str().with_context(|| "Can't get latest activity id."))
        .map(|v| v.to_string())
}

#[derive(Debug)]
pub enum NCEvent {
    Create(String),
    Delete(String),
    Modify(String),
    Move(String, String),
}

pub async fn get_ncevents(nc_info: &NCInfo, nc_state: &mut NCState) -> Result<Vec<NCEvent>> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = OCS_ROOT
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let res = Client::new()
        .request(Method::GET, url.as_str())
        .query(&[
            ("since", nc_state.latest_activity_id.to_string().as_str()),
            ("sort", "asc"),
        ])
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .header("OCS-APIRequest", "true")
        .send()
        .await?;

    let s = res.status();
    if !s.is_success() {
        if s.as_u16() == 304 {
            return Ok(vec![]);
        } else {
            return Err(BadStatusError(s.as_u16()).into());
        }
    }

    let latest_activity_id = res
        .headers()
        .get("X-Activity-Last-Given")
        .with_context(|| format!("Can't get latest activity id."))
        .and_then(|v| v.to_str().with_context(|| "Can't get latest activity id."))
        .map(|v| v.to_string())?;

    let text = res.text_with_charset("utf-8").await?;

    // debug!("{}", text);

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = ncevents_xml2responses(&document)?;

    nc_state.latest_activity_id = latest_activity_id;

    Ok(responses)
}

static RE_FILE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^file.*").unwrap());
static RE_NEWFILE: Lazy<Regex> = Lazy::new(|| Regex::new("^newfile.*").unwrap());
static RE_OLDFILE: Lazy<Regex> = Lazy::new(|| Regex::new("^oldfile.*").unwrap());

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ActivityType {
    FileCreated,
    FileChanged,
    FileDeleted,
}

fn ncevents_xml2responses(document: &roxmltree::Document) -> Result<Vec<NCEvent>> {
    let data = document
        .root_element()
        .children()
        .filter(|n| n.tag_name().name() == "data")
        .nth(0)
        .with_context(|| InvalidXMLError)?;

    let res = data
        .children()
        .map(|n| {
            if n.tag_name().name() != "element" {
                return Vec::new();
            }

            let mut files = Vec::new();
            let mut new_files = Vec::new();
            let mut old_files = Vec::new();
            let mut activity_type = None;

            for m in n.children() {
                if m.tag_name().name() == "type" {
                    activity_type = match m.text() {
                        // file_restored must be distinguished from file_created!!!!
                        // Because if you restored folder, its children will not be restored
                        // when file_restored be dealed as file_created.
                        // on hold.
                        Some("file_created") | Some("file_restored") => {
                            Some(ActivityType::FileCreated)
                        }
                        Some("file_changed") => Some(ActivityType::FileChanged),
                        Some("file_deleted") => Some(ActivityType::FileDeleted),
                        _ => None,
                    };
                }
            }

            for m in n.descendants() {
                match m.tag_name().name() {
                    s if RE_FILE.is_match(s) => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "path" => {
                                    let path = d.text().unwrap_or("");
                                    let path = add_head_slash(path);
                                    files.push(path);
                                }
                                _ => (),
                            }
                        }
                    }
                    s if RE_NEWFILE.is_match(s) => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "path" => {
                                    let path = d.text().unwrap_or("");
                                    let path = add_head_slash(path);
                                    new_files.push(path);
                                }
                                _ => (),
                            }
                        }
                    }
                    s if RE_OLDFILE.is_match(s) => {
                        for d in m.descendants() {
                            match d.tag_name().name() {
                                "path" => {
                                    let path = d.text().unwrap_or("");
                                    let path = add_head_slash(path);
                                    old_files.push(path);
                                }
                                _ => (),
                            }
                        }
                    }
                    _ => (),
                }
            }

            /*
            debug!(
                "{:?} {:?} {:?} {:?}",
                activity_type, files, new_files, old_files
            );
            */

            old_files.sort_by(|a, b| a.len().cmp(&b.len()).reverse());

            match activity_type {
                Some(ActivityType::FileCreated) => {
                    files.into_iter().map(|f| NCEvent::Create(f)).collect()
                }
                Some(ActivityType::FileDeleted) => {
                    files.into_iter().map(|f| NCEvent::Delete(f)).collect()
                }
                Some(ActivityType::FileChanged) => {
                    if new_files.len() > 0 {
                        let new_file = new_files.into_iter().nth(0).unwrap();
                        old_files
                            .into_iter()
                            .map(|f| NCEvent::Move(f, new_file.clone()))
                            .collect()
                    } else {
                        files.into_iter().map(|f| NCEvent::Modify(f)).collect()
                    }
                }
                _ => Vec::new(),
            }
        })
        .flatten()
        .collect();

    Ok(res)
}

fn touch_targets(update_targets: Vec<WeakEntry>, local_info: &LocalInfo) -> anyhow::Result<()> {
    for target in update_targets.into_iter() {
        if let Some(e) = target.upgrade() {
            let path = Entry::get_path(&e)?;
            let mut e_ref = e.lock().map_err(|_| LockError)?;
            let res = touch_entry(&path, e_ref.type_.is_file(), local_info);
            match res {
                Ok(_) => {
                    e_ref.status = EntryStatus::UpToDate;
                }
                Err(er) => {
                    e_ref.status = EntryStatus::Error;
                    // return Err(e);
                    debug!("{}", er);
                }
            }
        }
    }

    Ok(())
}

pub async fn update_tree(
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    nc_events: Vec<NCEvent>,
    root_entry: &ArcEntry,
    stash: bool,
) -> Result<Vec<WeakEntry>> {
    let mut download_targets = Vec::new();

    for event in nc_events.into_iter() {
        {
            let root_ref = root_entry.lock().map_err(|_| LockError)?;
            debug!("\n{}", root_ref.get_tree());
        }

        match event {
            NCEvent::Create(path) => {
                debug!("Create {}", path);
                // let path = correct_nc_path(nc_info, &path).await;
                let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
                if Entry::get(root_entry, &path)?.is_some() {
                    continue;
                }
                let name = path2name(&path);
                let type_ = if ncpath_is_file(nc_info, &path).await {
                    EntryType::File { etag: None }
                } else {
                    EntryType::Directory
                };
                let new_entry = Arc::new(Mutex::new(Entry::new(name, type_)));
                let new_entry_w = Arc::downgrade(&new_entry);
                download_targets.push(new_entry_w);
                let update_targets =
                    Entry::append(root_entry, &path, new_entry, AppendMode::Create, false)?;
                let filop_res = touch_targets(update_targets, local_info);
                if let Err(e) = filop_res {
                    info!("{}", e);
                }
            }
            NCEvent::Delete(path) => {
                debug!("Delete {}", path);
                // let path = correct_nc_path(nc_info, &path).await?;
                let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
                let mut root_ref = root_entry.lock().map_err(|_| LockError)?;
                let _ = root_ref.pop(&path)?;
                let filop_res = remove_entry(&path, stash, local_info);
                if let Err(e) = filop_res {
                    info!("{}", e);
                }
            }
            NCEvent::Modify(path) => {
                debug!("Modify {}", path);
                // let path = correct_nc_path(nc_info, &path).await;
                let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
                let w = Entry::get(root_entry, &path)?;
                /*
                if w.is_none() {
                    let p = add_last_slash(&path);
                    debug!("{} @ Modify", p);
                    let v = Entry::get(root_entry, &p)?;
                    let v = if v.is_none() {
                        let p = drop_slash(&path, &RE_HAS_LAST_SLASH);
                        debug!("{} @ Modify", p);
                        Entry::get(root_entry, &p)?
                    } else {
                        v
                    }
                    .with_context(|| InvalidPathError(format!("{}", path)))?;
                    let v_arc = v
                        .upgrade()
                        .with_context(|| InvalidPathError(format!("{}", path)))?;
                    debug!("try_fix_entry_type@modify");
                    let _ = try_fix_entry_type(&v_arc, &path, nc_info, local_info).await?;
                    w = Some(v);
                }
                */
                if_chain! {
                    if let Some(w) = w;
                    if let Some(e) = w.upgrade();
                    then {
                        let is_file = {
                            let mut e_ref = e.lock().map_err(|_| LockError)?;
                            debug!("NeedUpdate substituted.");
                            e_ref.status = EntryStatus::NeedUpdate;
                            e_ref.type_.is_file()
                        };
                        if is_file {
                            download_targets.push(Arc::downgrade(&e));
                        }
                    }
                }
            }
            NCEvent::Move(from_path, to_path) => {
                debug!("Move {} => {}", from_path, to_path);
                // let to_path = correct_nc_path(nc_info, &to_path).await;
                let to_path = drop_slash(&to_path, &RE_HAS_LAST_SLASH);
                let from_path = drop_slash(&from_path, &RE_HAS_LAST_SLASH);
                let target_w = {
                    let mut root_ref = root_entry.lock().map_err(|_| LockError)?;
                    root_ref.pop(&from_path)?
                }
                .with_context(|| InvalidPathError(format!("{} => {}", from_path, to_path)));

                let target = match target_w {
                    Ok(v) => v,
                    Err(e) => {
                        info!("{}", e);
                        continue;
                    }
                };

                /*
                debug!("try_fix_entry_type@move");
                let fixed_to_path = fix_and_concat_nc_path(&to_path, root_entry, &target)?;
                let have_to_download =
                    try_fix_entry_type(&target, &fixed_to_path, nc_info, local_info).await?;
                if have_to_download {
                    let w = Arc::downgrade(&target);
                    download_targets.push(w);
                }
                */

                let update_targets =
                    Entry::append(root_entry, &to_path, target.clone(), AppendMode::Move, true)?;

                touch_targets(update_targets, local_info)?;

                let res = move_entry(&from_path, &to_path, stash, local_info);
                match res {
                    Err(er) => {
                        /*
                        let mut t_ref = target.lock().map_err(|_| LockError)?;
                        t_ref.status = EntryStatus::Error;
                        // return Err(e);
                        debug!("{}", er);
                        */
                        info!("{}", er);
                    }
                    Ok(_) => {
                        debug!("try_fix_entry_type@move");
                        let path = Entry::get_path(&target)?;
                        let have_to_download =
                            try_fix_entry_type(&target, &path, nc_info, local_info).await?;
                        if have_to_download {
                            let w = Arc::downgrade(&target);
                            download_targets.push(w);
                        }
                    }
                }
            }
        }
    }
    Ok(download_targets)
}

// Nextcloud API がクソすぎるのために必要...かも...
/*
fn fix_and_concat_nc_path(
    nc_path: &str,
    root_entry: &ArcEntry,
    target: &ArcEntry,
) -> Result<String> {
    let already_exist = Entry::get(root_entry, add_last_slash(nc_path).as_str())?.is_some();

    if already_exist {
        let t = drop_slash(nc_path, &RE_HAS_LAST_SLASH);
        let target_ref = target.lock().map_err(|_| LockError)?;
        Ok(format!("{}/{}", t, target_ref.get_name()))
    } else {
        Ok(nc_path.to_string())
    }
}
*/

async fn try_fix_entry_type(
    target: &ArcEntry,
    path: &str,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
) -> Result<bool> {
    // let path = Entry::get_path(target)?; // @Move, target has no parent.
    let res = from_nc(nc_info, path).await;

    let nc_entry = match res {
        Ok(entry) => entry,
        Err(_) => return Ok(false),
    };

    let mut target_ref = target.lock().map_err(|_| LockError)?;
    if target_ref.type_.is_same_type(&nc_entry.type_) {
        return Ok(false);
    }

    target_ref.status = EntryStatus::NeedUpdate;

    target_ref.type_ = if target_ref.type_.is_file() {
        EntryType::Directory
    } else {
        EntryType::File { etag: None }
    };

    // let path = drop_slash(&path, &RE_HAS_LAST_SLASH);
    debug!("{}", path);
    remove_entry(&path, false, local_info)?;
    touch_entry(&path, target_ref.type_.is_file(), local_info)?;

    Ok(target_ref.type_.is_file())
}
