//! Folder culling commands; filesystem transactions stay independent of ordinary edit Undo.
use super::{CommandSpec, always, bad, cmd, str_param};
use crate::{Result, Selection, Session};
use lightcraft_catalog::PhotoId;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

fn validate(p: &Value, command: &str) -> Result<()> {
    if !p.is_object() {
        return Err(bad(command, "parameters must be an object"));
    }
    for key in ["folder", "journal_path"] {
        if p.get(key).is_some_and(|v| !v.as_str().is_some_and(|s| !s.trim().is_empty())) {
            return Err(bad(command, format!("{key} must be a nonempty string")));
        }
    }
    Ok(())
}

fn root(s: &Session, p: &Value, command: &str) -> Result<PathBuf> {
    validate(p, command)?;
    str_param(p, "folder")
        .or_else(|| s.browse.as_ref().map(|b| b.path.as_str()))
        .map(PathBuf::from)
        .ok_or_else(|| bad(command, "choose a folder or provide `folder`"))
}

pub(crate) fn recover_folder(s: &mut Session, path: &str) -> Result<Value> {
    Ok(json!({"transactions": crate::culling::recover(s, Path::new(path))?}))
}

fn reject(s: &mut Session, p: &Value) -> Result<Value> {
    validate(p, "cull.reject")?;
    if p.get("id").is_some_and(|v| v.as_u64().is_none()) {
        return Err(bad("cull.reject", "id must be a photo id"));
    }
    if p.get("id").is_some() && p.get("ids").is_some() {
        return Err(bad("cull.reject", "provide id or ids, not both"));
    }
    if let Some(ids) = p.get("ids")
        && !ids.as_array().is_some_and(|a| a.len() <= 10_000 && a.iter().all(|v| v.as_u64().is_some()))
    {
        return Err(bad("cull.reject", "ids must be an array of photo ids"));
    }
    let ids = s.targets(p);
    let order = s.visible_cloned();
    let index = s.active().and_then(|id| order.iter().position(|i| *i == id)).unwrap_or(0);
    let tx = crate::culling::reject(s, &ids)?;
    let remaining: Vec<PhotoId> = s.visible_cloned().into_iter().filter(|id| !ids.contains(id)).collect();
    s.selection = remaining.get(index.min(remaining.len().saturating_sub(1))).copied().map(Selection::single).unwrap_or_default();
    Ok(json!({"transaction_id":tx.transaction_id,"journal_path":tx.journal_path,"moved":crate::culling::moved(&tx,false),"active":s.active()}))
}

fn undo(s: &mut Session, p: &Value) -> Result<Value> {
    let path = root(s, p, "cull.undo")?;
    let tx = crate::culling::undo(s, &path, str_param(p, "journal_path"))?;
    let moved = crate::culling::moved(&tx, true);
    // Manifest ids may have changed after reopening a catalog; return current matching ids.
    let ids = crate::culling::reconcile(s, &tx, true)?;
    s.selection = Selection { active: ids.first().copied(), ids: ids.clone() };
    Ok(json!({"transaction_id":tx.transaction_id,"journal_path":tx.journal_path,"moved":moved,"restored_ids":ids,"active":s.active()}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "cull.reject",
            "Move Rejected Photos",
            [],
            None,
            "{ids?} — move selected original files and unambiguous XMP/DOP sidecars into Rejected; collision refusal and durable recovery → {transaction_id,journal_path,moved,active}",
            always,
            reject
        ),
        cmd!(
            "cull.undo",
            "Undo Folder Rejection",
            [],
            None,
            "{folder?,journal_path?} — restore the latest rejection, or the specified journal, without overwriting → {transaction_id,journal_path,moved,restored_ids,active}",
            always,
            undo
        ),
        cmd!(query "cull.history","Folder Culling History",[],None,"{folder?} → {transactions:[{transaction_id,journal_path,state}]}",always,|s,p| Ok(json!({"transactions":crate::culling::history(&root(s,p,"cull.history")?)?}))),
        cmd!(
            "cull.recover",
            "Recover Folder Rejection",
            [],
            None,
            "{folder?} — recover interrupted moves and reconcile catalog paths → {transactions}",
            always,
            |s, p| {
                let path = root(s, p, "cull.recover")?;
                recover_folder(s, &path.to_string_lossy())
            }
        ),
    ]
}
