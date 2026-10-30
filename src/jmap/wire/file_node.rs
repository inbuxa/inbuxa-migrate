/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 * SPDX-FileCopyrightText: 2026 Coffey Labs LLC <legal@coffeylabs.org>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

use super::common::bool_or_true;
use super::{JmapId, UtcDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// How a server spells a FileNode. Servers on the 0.16 line, inbuxa among
/// them, name the kind `nodeType` (`file`, `directory`, `symlink`) and a
/// file's media type `type`. draft-ietf-jmap-filenode-18, which Stalwart 1.0
/// follows, says `@type` (`File`, `Directory`, `SymLink`) and `mediaType`,
/// and adds `executable`. Each refuses the other's names on a write and
/// ignores them on a get, so a get asks for both and a write uses the
/// server's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Legacy,
    Draft18,
}

/// A FileNode, in either shape, read into this one; a node with neither
/// `@type` nor `nodeType` is a file if it has a blob, a symlink if it has a
/// target, and a directory otherwise.
pub fn to_draft18(node: &mut Map<String, Value>) {
    let kind = match node.remove("@type") {
        Some(Value::String(t)) => t,
        _ => match node.remove("nodeType") {
            Some(Value::String(t)) => t,
            _ if node.get("blobId").is_some_and(|v| !v.is_null()) => "file".to_owned(),
            _ if node.get("target").is_some_and(|v| !v.is_null()) => "symlink".to_owned(),
            _ => "directory".to_owned(),
        },
    };
    node.remove("nodeType");
    let kind = match kind.to_ascii_lowercase().as_str() {
        "file" => "File".to_owned(),
        "directory" => "Directory".to_owned(),
        "symlink" => "SymLink".to_owned(),
        _ => kind,
    };
    node.insert("@type".to_owned(), Value::String(kind));
    if let Some(media_type) = node.remove("type") {
        node.entry("mediaType").or_insert(media_type);
    }
}

/// A FileNode, or a patch to one, written as `shape` spells it.
pub fn for_server(node: &mut Map<String, Value>, shape: Shape) {
    if shape == Shape::Draft18 {
        return;
    }
    if let Some(Value::String(kind)) = node.remove("@type") {
        node.insert(
            "nodeType".to_owned(),
            Value::String(kind.to_ascii_lowercase()),
        );
    }
    if let Some(media_type) = node.remove("mediaType") {
        node.insert("type".to_owned(), media_type);
    }
    node.remove("executable");
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "Map<String, Value>")]
pub struct FileNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JmapId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<JmapId>,

    pub name: String,

    #[serde(with = "time::serde::rfc3339")]
    pub created: UtcDate,

    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub modified: Option<UtcDate>,

    #[serde(flatten)]
    pub kind: NodeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "@type")]
pub enum NodeKind {
    File(FileProps),
    Directory(DirectoryProps),
    SymLink(SymLinkProps),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileProps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_id: Option<JmapId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    #[serde(default, deserialize_with = "bool_or_false")]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryProps {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    #[serde(default = "default_true", deserialize_with = "bool_or_true")]
    pub is_subscribed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymLinkProps {
    pub target: Vec<String>,
}

/// `FileNode`'s fields, read once the node is in draft-18's names.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Draft18Node {
    #[serde(default)]
    id: Option<JmapId>,
    #[serde(default)]
    parent_id: Option<JmapId>,
    name: String,
    #[serde(with = "time::serde::rfc3339")]
    created: UtcDate,
    #[serde(default, with = "time::serde::rfc3339::option")]
    modified: Option<UtcDate>,
    #[serde(flatten)]
    kind: NodeKind,
}

impl TryFrom<Map<String, Value>> for FileNode {
    type Error = serde_json::Error;

    fn try_from(mut node: Map<String, Value>) -> Result<Self, Self::Error> {
        to_draft18(&mut node);
        let n: Draft18Node = serde_json::from_value(Value::Object(node))?;
        Ok(FileNode {
            id: n.id,
            parent_id: n.parent_id,
            name: n.name,
            created: n.created,
            modified: n.modified,
            kind: n.kind,
        })
    }
}

impl NodeKind {
    pub fn db_name(&self) -> &'static str {
        match self {
            NodeKind::File(_) => "file",
            NodeKind::Directory(_) => "directory",
            NodeKind::SymLink(_) => "symlink",
        }
    }
}

fn bool_or_false<'de, D>(d: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(d)?.unwrap_or_default())
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(v: Value) -> FileNode {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn both_shapes_read_the_same() {
        let created = "2022-02-02T02:02:02Z";
        let draft = node(
            json!({"id": "F1", "@type": "File", "name": "a.sh", "blobId": "B1",
            "mediaType": "text/x-sh", "executable": true, "created": created}),
        );
        let legacy = node(
            json!({"id": "F1", "nodeType": "file", "name": "a.sh", "blobId": "B1",
            "type": "text/x-sh", "created": created}),
        );
        for n in [&draft, &legacy] {
            let NodeKind::File(f) = &n.kind else {
                panic!("{n:?}")
            };
            assert_eq!(f.media_type.as_deref(), Some("text/x-sh"));
            assert_eq!(f.blob_id.as_ref().map(|b| b.0.as_str()), Some("B1"));
        }
        let link = node(json!({"id": "L", "nodeType": "symlink", "name": "l",
            "target": ["a"], "created": created}));
        assert!(matches!(link.kind, NodeKind::SymLink(_)));
        let dir = node(json!({"id": "D", "name": "d", "created": created}));
        assert!(matches!(dir.kind, NodeKind::Directory(_)));
        assert!(
            serde_json::from_value::<FileNode>(json!({"id": "S", "nodeType": "socket",
            "name": "s", "created": created}))
            .is_err()
        );
    }

    #[test]
    fn a_legacy_server_gets_its_own_names() {
        let mut m = json!({"@type": "File", "name": "a.sh", "blobId": "B1",
            "mediaType": "text/x-sh", "executable": true});
        let Value::Object(map) = &mut m else {
            unreachable!()
        };
        let mut draft = map.clone();
        for_server(&mut draft, Shape::Draft18);
        assert_eq!(&draft, map);
        for_server(map, Shape::Legacy);
        assert_eq!(
            m,
            json!({"nodeType": "file", "name": "a.sh", "blobId": "B1",
            "type": "text/x-sh"})
        );
    }
}
