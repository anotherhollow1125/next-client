use regex::Regex;
use reqwest::{Client, Method, Url};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::{fs, io};
use urlencoding::decode;

use crate::*;

pub struct NCInfo {
    pub username: String,
    pub password: String,
    pub host: String,
    pub root_path: String,
}

impl NCInfo {
    pub fn new(username: String, password: String, host: String, root_path: String) -> Self {
        let host = fix_host(&host);
        let root_path = fix_root(&root_path);
        Self {
            username,
            password,
            host,
            root_path,
        }
    }
}

pub async fn from_nc(nc_info: &NCInfo, target: &str) -> anyhow::Result<Entry> {
    let target = add_head_slash(&target);
    let responses = comm_nc(nc_info, &target).await?;

    let target_res = responses
        .into_iter()
        .filter(|r| {
            let a = drop_slash(&r.path, &RE_HAS_LAST_SLASH);
            let b = drop_slash(&target, &RE_HAS_LAST_SLASH);
            a == b
        })
        .nth(0);

    target_res.ok_or(anyhow!("Can not found target Entry."))
}

pub async fn from_nc_all(
    nc_info: &NCInfo,
    target: &str,
) -> anyhow::Result<(RcEntry, HashMap<String, RcEntry>)> {
    let target = add_head_slash(&target);
    let top_entry = from_nc(nc_info, &target).await?;
    let top_path = top_entry.path.clone();
    let top_entry = Rc::new(RefCell::new(top_entry));

    let mut book = HashMap::new();
    book.insert(top_path, top_entry.clone());

    let mut stack = vec![top_entry.clone()];
    while let Some(entry) = stack.pop() {
        get_children(nc_info, &mut entry.borrow_mut(), &mut book, &mut stack).await?;
    }

    Ok((top_entry, book))
}

async fn get_children(
    nc_info: &NCInfo,
    parent: &mut Entry,
    book: &mut HashMap<String, RcEntry>,
    stack: &mut Vec<RcEntry>,
) -> anyhow::Result<()> {
    if parent.type_.is_file() {
        return Ok(());
    }

    let children_entries = comm_nc(nc_info, &parent.path)
        .await?
        .into_iter()
        .filter(|c| c.path != parent.path);

    let mut children = Vec::new();
    for c in children_entries {
        let path = c.path.clone();
        let c = Rc::new(RefCell::new(c));
        let w = Rc::downgrade(&c);

        book.insert(path, c.clone());
        if !c.borrow().type_.is_file() {
            stack.push(c);
        }

        children.push(w);
    }

    parent.type_ = EntryType::Directory { children };

    Ok(())
}

async fn comm_nc(nc_info: &NCInfo, target: &str) -> anyhow::Result<Vec<Entry>> {
    /*
    let host = fix_host(host);
    let root_path = fix_root(root_path);
    */
    let target = add_head_slash(target);
    // let target = drop_slash(target, &RE_HAS_HEAD_SLASH);

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
        return Err(anyhow!("status: {}", res.status()));
    }

    let text = res.text_with_charset("utf-8").await?;

    // println!("{}", text);

    let document: roxmltree::Document = roxmltree::Document::parse(&text)?;
    let responses = xml2responses(&document, &nc_info.root_path);

    Ok(responses)
}

fn xml2responses(document: &roxmltree::Document, root_path: &str) -> Vec<Entry> {
    document
        .root_element()
        .children()
        .map(|n| {
            if n.tag_name().name() != "response" {
                return None;
            }

            let mut name_w = None;
            let mut path_w = None;
            let mut etag_w = None;
            let mut type_w = None;

            for m in n.children() {
                match m.tag_name().name() {
                    "href" => {
                        if let Some(href) = m.text() {
                            let path = href.replace(&root_path, "");
                            let path = decode(&path).ok()?;
                            let path_name = drop_slash(&path, &RE_HAS_LAST_SLASH);
                            name_w = Some(path_name.split("/").last().unwrap_or("").to_string());
                            path_w = Some(path_name);
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
                                        Some(ref s) if s != &"" => Some(EntryType::File),
                                        _ => Some(EntryType::Directory {
                                            children: Vec::new(),
                                        }),
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
                if let Some(path) = path_w;
                if let Some(etag) = etag_w;
                if let Some(type_) = type_w;
                then {
                    let (name, path) = match &type_ {
                        &EntryType::File => (name, path),
                        _ => (add_last_slash(&name), add_last_slash(&path)),
                    };

                    // ルートディレクトリに限らず、親ディレクトリとETagが一致する現象が存在
                    // 一意性をもたせるのにETagは不十分そう
                    /*
                    let etag = if name == "" {
                        "".to_string()
                    } else {
                        etag
                    };
                    */

                    Some(Entry {
                        name,
                        path,
                        etag,
                        type_,
                    })
                } else {
                    None
                }
            }
        })
        .filter_map(|v| v)
        .collect()
}

pub async fn make_entry(
    target_path: &str,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
    book: &mut HashMap<String, RcEntry>,
) -> anyhow::Result<()> {
    let target_path = add_head_slash(target_path);
    let re = Regex::new("(.*)/.*?$").unwrap();

    if book.get(&target_path).is_none() {
        let mut path = re.replace(&target_path, "$1").to_string();
        while path.len() > 0 {
            if book.get(&path).is_some() {
                let (_, sub_book) = from_nc_all(nc_info, &path).await?;
                sub_book.into_iter().for_each(|(k, v)| {
                    book.insert(k, v);
                });
                break;
            }

            path = re.replace(&path, "$1").to_string();
        }
    }

    if book.get(&target_path).is_none() {
        return Err(anyhow!("No such target file."));
    }
    let target_entry = book.get(&target_path).unwrap();

    let dir_path = re.replace(&target_path, "$1").to_string();
    let dir_path = format!("{}{}", local_info.root_path, dir_path);

    fs::DirBuilder::new()
        .recursive(true)
        .create(dir_path)
        .unwrap();

    let t = target_entry.borrow();
    if t.type_.is_file() {
        download_file(&t, &nc_info, local_info).await?;
    }

    Ok(())
}

async fn download_file(
    target: &Entry,
    nc_info: &NCInfo,
    local_info: &LocalInfo,
) -> anyhow::Result<()> {
    let mut url = Url::parse(&nc_info.host)?;
    let path_v = format!("{}{}", nc_info.root_path, target.path)
        .split("/")
        .map(|v| v.to_string())
        .collect::<Vec<String>>();
    url.path_segments_mut().unwrap().extend(path_v);

    let data_res = Client::new()
        .request(Method::GET, url.as_str())
        .basic_auth(&nc_info.username, Some(&nc_info.password))
        .send()
        .await?;

    let bytes = data_res.bytes().await?;
    let filename = format!("{}{}", local_info.root_path, target.path);
    let mut out = fs::File::create(filename)?;
    io::copy(&mut bytes.as_ref(), &mut out)?;

    Ok(())
}
