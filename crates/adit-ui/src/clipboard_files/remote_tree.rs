//! A clipboard offer is an immutable read-only tree. Neither a FUSE inode nor
//! a native file promise may turn a server-controlled name into a local path.
use super::{ChunkBridge, ChunkError, ClipFile, MAX_CLIPBOARD_FILES};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

pub(super) const ROOT: u64 = 1;
const MAX_DEPTH: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const MAX_CHUNK: u32 = 1024 * 1024;

#[derive(Debug, Clone)]
pub(super) struct Node {
    pub ino: u64,
    pub parent: u64,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub remote_index: Option<u32>,
}

#[derive(Debug)]
pub(super) struct RemoteTree {
    nodes: Vec<Node>,
    by_name: BTreeMap<(u64, String), u64>,
}

impl RemoteTree {
    pub fn new(files: &[ClipFile]) -> Result<Self, &'static str> {
        if files.is_empty() || files.len() > MAX_CLIPBOARD_FILES {
            return Err("远程剪贴板文件数量无效");
        }
        let mut tree = Self {
            nodes: vec![Node {
                ino: ROOT,
                parent: ROOT,
                name: String::new(),
                is_dir: true,
                size: 0,
                remote_index: None,
            }],
            by_name: BTreeMap::new(),
        };
        let mut paths = BTreeMap::new();
        let mut explicit = HashSet::new();
        for (index, file) in files.iter().enumerate() {
            let parts = relative_parts(&file.name)?;
            let path = parts.join("/");
            if !explicit.insert(path.clone()) || file.size > i64::MAX as u64 {
                return Err("远程剪贴板包含重复名称或无效长度");
            }
            let mut parent = ROOT;
            let mut current = String::new();
            for (depth, part) in parts.iter().enumerate() {
                if !current.is_empty() {
                    current.push('/');
                }
                current.push_str(part);
                let last = depth + 1 == parts.len();
                let is_dir = !last || file.is_dir;
                if let Some(ino) = paths.get(&current).copied() {
                    let node = &tree.nodes[(ino - 1) as usize];
                    if !node.is_dir || !is_dir {
                        return Err("远程剪贴板文件与目录名称冲突");
                    }
                    parent = ino;
                    continue;
                }
                if tree.nodes.len() > MAX_CLIPBOARD_FILES {
                    return Err("远程剪贴板目录层级展开过大");
                }
                let ino = tree.nodes.len() as u64 + 1;
                tree.nodes.push(Node {
                    ino,
                    parent,
                    name: (*part).to_owned(),
                    is_dir,
                    size: if is_dir { 0 } else { file.size },
                    remote_index: (!is_dir).then_some(index as u32),
                });
                paths.insert(current.clone(), ino);
                tree.by_name.insert((parent, (*part).to_owned()), ino);
                parent = ino;
            }
        }
        Ok(tree)
    }

    pub fn get(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(usize::try_from(ino.checked_sub(1)?).ok()?)
    }

    pub fn lookup(&self, parent: u64, name: &str) -> Option<&Node> {
        self.by_name
            .get(&(parent, name.to_owned()))
            .and_then(|ino| self.get(*ino))
    }

    pub fn children(&self, parent: u64) -> impl Iterator<Item = &Node> {
        self.by_name
            .range((parent, String::new())..)
            .take_while(move |((owner, _), _)| *owner == parent)
            .filter_map(|(_, ino)| self.get(*ino))
    }
}

fn relative_parts(name: &str) -> Result<Vec<&str>, &'static str> {
    if name.is_empty() || name.len() > MAX_PATH_BYTES || name.starts_with(['/', '\\']) {
        return Err("远程剪贴板必须使用有界相对文件名");
    }
    let parts: Vec<_> = name.split(['/', '\\']).collect();
    if parts.len() > MAX_DEPTH
        || parts.iter().any(|part| {
            part.is_empty()
                || matches!(*part, "." | "..")
                || part.len() > 255
                || part.ends_with([' ', '.'])
                || part
                    .chars()
                    .any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        })
    {
        return Err("远程剪贴板文件名包含不安全的路径片段");
    }
    Ok(parts)
}

pub(super) type Requester = Arc<dyn Fn(u32, u32, u64, u32) + Send + Sync>;

pub(super) struct RemoteReader {
    bridge: ChunkBridge,
    request: Requester,
    chunk: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReadError {
    NotAFile,
    InvalidReply,
    Transport(ChunkError),
}

impl RemoteReader {
    pub fn new(bridge: ChunkBridge, request: Requester, chunk: u32) -> Self {
        Self {
            bridge,
            request,
            chunk: chunk.clamp(1, MAX_CHUNK),
        }
    }

    /// Called by an OS worker only. An empty response before the declared EOF
    /// is a failed transfer, never successful truncation.
    pub fn read(&self, node: &Node, offset: u64, length: u32) -> Result<Vec<u8>, ReadError> {
        let index = node.remote_index.ok_or(ReadError::NotAFile)?;
        let length = u64::from(length.min(self.chunk)).min(node.size.saturating_sub(offset)) as u32;
        if length == 0 {
            return Ok(Vec::new());
        }
        let stream = self.bridge.begin().map_err(ReadError::Transport)?;
        (self.request)(stream, index, offset, length);
        let bytes = self.bridge.wait(stream).map_err(ReadError::Transport)?;
        if bytes.is_empty() || bytes.len() > length as usize {
            return Err(ReadError::InvalidReply);
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> ClipFile {
        ClipFile {
            name: name.into(),
            size,
            is_dir: false,
        }
    }

    #[test]
    fn metadata_builds_directories_without_fetching_file_content_and_preserves_wire_indices() {
        let tree = RemoteTree::new(&[file("docs\\b.txt", 3), file("a.bin", 4)]).unwrap();
        let roots: Vec<_> = tree.children(ROOT).map(|node| node.name.as_str()).collect();
        assert_eq!(roots, ["a.bin", "docs"]);
        let docs = tree.lookup(ROOT, "docs").unwrap();
        assert!(docs.is_dir);
        let b = tree.lookup(docs.ino, "b.txt").unwrap();
        assert_eq!((b.remote_index, b.size, b.parent), (Some(0), 3, docs.ino));
        assert_eq!(tree.get(b.ino).unwrap().name, "b.txt");
        assert_eq!(tree.lookup(ROOT, "a.bin").unwrap().remote_index, Some(1));
        assert!(tree.get(0).is_none());
    }

    #[test]
    fn untrusted_paths_conflicting_nodes_and_unbounded_trees_are_rejected() {
        for path in [
            "../escape",
            "a/../../escape",
            "/absolute",
            "\\\\server\\share",
            "C:\\private",
            "a//b",
            "a/./b",
            "a\0b",
            "a:file",
            "trailing.",
        ] {
            assert!(RemoteTree::new(&[file(path, 1)]).is_err(), "{path:?}");
        }
        assert!(RemoteTree::new(&[file("a", 1), file("a\\b", 1)]).is_err());
        assert!(RemoteTree::new(&[file("a\\b", 1), file("a", 1)]).is_err());
        assert!(RemoteTree::new(&[file("a", 1), file("a", 1)]).is_err());
        assert!(RemoteTree::new(&[file("a", u64::MAX)]).is_err());
        assert!(RemoteTree::new(&[file(&vec!["a"; 33].join("/"), 1)]).is_err());
    }

    #[test]
    fn bounded_reads_preserve_binary_data_and_refusal_is_not_end_of_file() {
        let bridge = ChunkBridge::new();
        let reply = bridge.clone();
        let reader = RemoteReader::new(
            bridge,
            Arc::new(move |stream, index, offset, length| {
                assert_eq!((index, offset, length), (0, 1, 2));
                reply.deliver(stream, Some(vec![0, 255]));
            }),
            2,
        );
        let tree = RemoteTree::new(&[file("binary", 4)]).unwrap();
        let node = tree.lookup(ROOT, "binary").unwrap();
        assert_eq!(reader.read(node, 1, 100).unwrap(), [0, 255]);
        assert_eq!(reader.read(node, 4, 100).unwrap(), []);
        assert_eq!(
            reader.read(tree.get(ROOT).unwrap(), 0, 10),
            Err(ReadError::NotAFile)
        );

        for data in [None, Some(vec![]), Some(vec![1; 5])] {
            let bridge = ChunkBridge::new();
            let reply = bridge.clone();
            let reader = RemoteReader::new(
                bridge,
                Arc::new(move |stream, _, _, _| {
                    reply.deliver(stream, data.clone());
                }),
                4,
            );
            assert!(reader.read(node, 0, 4).is_err());
        }
    }

    #[test]
    fn closing_one_offer_cannot_redirect_reads_to_another_session() {
        let tree = RemoteTree::new(&[file("file", 1)]).unwrap();
        let node = tree.lookup(ROOT, "file").unwrap();
        let first = ChunkBridge::new();
        first.close();
        let first = RemoteReader::new(
            first,
            Arc::new(|_, _, _, _| panic!("closed offer requested bytes")),
            4,
        );
        assert_eq!(
            first.read(node, 0, 1),
            Err(ReadError::Transport(ChunkError::Disconnected))
        );
        let second = ChunkBridge::new();
        let reply = second.clone();
        let second = RemoteReader::new(
            second,
            Arc::new(move |id, _, _, _| {
                reply.deliver(id, Some(vec![7]));
            }),
            4,
        );
        assert_eq!(second.read(node, 0, 1).unwrap(), [7]);
    }
}
