//! Snapshot and request identity for remote clipboard files. IPC stream ids
//! belong to callers; wire ids belong to this channel and are never reused.
use adit_rdp_proto::ClipFile;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub generation: u64,
    pub stream_id: u32,
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundRead {
    pub generation: u64,
    pub wire_id: u32,
    pub index: u32,
    pub offset: u64,
    pub length: u32,
    pub data_id: u32,
}

#[derive(Debug)]
struct Snapshot {
    files: Vec<ClipFile>,
    data_id: u32,
}
#[derive(Debug)]
struct Pending {
    generation: u64,
    client_id: u32,
    length: u32,
}
#[derive(Debug, Default)]
pub(super) struct RemoteFiles {
    generation: u64,
    exhausted: bool,
    expects_files: bool,
    next_wire_id: u64,
    snapshot: Option<Snapshot>,
    pending: HashMap<u32, Pending>,
}

impl RemoteFiles {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn expects_files(&self) -> bool {
        self.expects_files
    }
    pub fn active(&self) -> bool {
        self.snapshot.is_some()
    }

    pub fn revoke(&mut self, expects_files: bool) -> (bool, Vec<Reply>) {
        let had_offer = self.snapshot.take().is_some() || !self.pending.is_empty();
        self.expects_files = expects_files;
        if let Some(next) = self.generation.checked_add(1).filter(|_| !self.exhausted) {
            self.generation = next;
        } else {
            self.exhausted = true;
            self.generation = 0;
        }
        let replies = self
            .pending
            .drain()
            .map(|(_, p)| Reply {
                generation: p.generation,
                stream_id: p.client_id,
                data: None,
            })
            .collect();
        (had_offer, replies)
    }

    pub fn install(&mut self, files: Vec<ClipFile>, data_id: Option<u32>) -> bool {
        // Without the peer's lock, a range request could read a replacement
        // clipboard before its FormatList reached us. Refuse unpinned offers.
        if !self.expects_files
            || self.exhausted
            || files.is_empty()
            || self.snapshot.is_some()
            || !self.pending.is_empty()
            || adit_rdp_proto::validate_clipboard_file_list(&files).is_err()
        {
            return false;
        }
        let Some(data_id) = data_id else {
            return false;
        };
        self.snapshot = Some(Snapshot { files, data_id });
        true
    }

    pub fn request(
        &mut self,
        generation: u64,
        client_id: u32,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<BoundRead, Reply> {
        let refuse = |data| Reply {
            generation,
            stream_id: client_id,
            data,
        };
        let Some(snapshot) = self
            .snapshot
            .as_ref()
            .filter(|_| generation != 0 && generation == self.generation)
        else {
            return Err(refuse(None));
        };
        let Some(file) = snapshot.files.get(index as usize).filter(|f| !f.is_dir) else {
            return Err(refuse(None));
        };
        if offset > file.size {
            return Err(refuse(None));
        }
        if length == 0 || offset == file.size {
            return Err(refuse(Some(Vec::new())));
        }
        let length =
            u64::from(length.min(adit_rdp_proto::FILE_CHUNK_BYTES)).min(file.size - offset) as u32;
        if self.pending.len() >= 256
            || self
                .pending
                .values()
                .any(|p| p.generation == generation && p.client_id == client_id)
        {
            return Err(refuse(None));
        }
        let Some(next) = self
            .next_wire_id
            .checked_add(1)
            .filter(|n| *n <= u64::from(u32::MAX))
        else {
            return Err(refuse(None));
        };
        self.next_wire_id = next;
        let wire_id = next as u32;
        self.pending.insert(
            wire_id,
            Pending {
                generation,
                client_id,
                length,
            },
        );
        Ok(BoundRead {
            generation,
            wire_id,
            index,
            offset,
            length,
            data_id: snapshot.data_id,
        })
    }

    pub fn current(&self, read: &BoundRead) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.data_id == read.data_id)
            && self.generation == read.generation
            && self
                .pending
                .get(&read.wire_id)
                .is_some_and(|p| p.generation == read.generation && p.length == read.length)
    }

    pub fn fail(&mut self, wire_id: u32) -> Option<Reply> {
        self.respond(wire_id, None)
    }
    pub fn respond(&mut self, wire_id: u32, data: Option<Vec<u8>>) -> Option<Reply> {
        let pending = self.pending.remove(&wire_id)?;
        let data = data.filter(|bytes| {
            self.active()
                && pending.generation == self.generation
                && !bytes.is_empty()
                && bytes.len() <= pending.length as usize
        });
        Some(Reply {
            generation: pending.generation,
            stream_id: pending.client_id,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn offer(state: &mut RemoteFiles, id: u32) -> u64 {
        state.revoke(true);
        assert!(state.install(
            vec![ClipFile {
                name: "synthetic".into(),
                size: 8,
                is_dir: false
            }],
            Some(id)
        ));
        state.generation()
    }
    #[test]
    fn old_request_cannot_use_new_offer_even_before_gui_poll() {
        let mut state = RemoteFiles::default();
        let a = offer(&mut state, 10);
        let old = state.request(a, 1, 0, 0, 3).unwrap();
        let b = offer(&mut state, 20);
        assert!(!state.current(&old));
        assert_eq!(state.request(a, 2, 0, 0, 3).unwrap_err().data, None);
        let new = state.request(b, 1, 0, 0, 3).unwrap();
        assert_eq!(new.data_id, 20);
        assert_ne!(old.wire_id, new.wire_id);
        assert!(state.respond(old.wire_id, Some(vec![99])).is_none());
        let reply = state.respond(new.wire_id, Some(vec![1, 2, 3])).unwrap();
        assert_eq!(
            (reply.generation, reply.stream_id, reply.data),
            (b, 1, Some(vec![1, 2, 3]))
        );
    }
    #[test]
    fn text_channel_reset_and_unlocked_offers_revoke_all_reads() {
        let mut state = RemoteFiles::default();
        let generation = offer(&mut state, 7);
        let read = state.request(generation, 5, 0, 0, 3).unwrap();
        let (had_offer, replies) = state.revoke(false);
        assert!(had_offer);
        assert_eq!(replies[0].generation, generation);
        assert_eq!(replies[0].data, None);
        assert!(!state.current(&read));
        assert!(state.respond(read.wire_id, Some(vec![3])).is_none());
        assert!(!state.install(
            vec![ClipFile {
                name: "synthetic".into(),
                size: 8,
                is_dir: false
            }],
            Some(8)
        ));
        state.revoke(true);
        assert!(!state.install(
            vec![ClipFile {
                name: "synthetic".into(),
                size: 8,
                is_dir: false
            }],
            None
        ));
    }
    #[test]
    fn forged_lengths_and_premature_eof_are_refusals() {
        let mut state = RemoteFiles::default();
        let generation = offer(&mut state, 7);
        let read = state.request(generation, 1, 0, 0, 3).unwrap();
        assert_eq!(
            state.respond(read.wire_id, Some(vec![1; 4])).unwrap().data,
            None
        );
        let read = state.request(generation, 2, 0, 0, 3).unwrap();
        assert_eq!(
            state.respond(read.wire_id, Some(Vec::new())).unwrap().data,
            None
        );
        assert_eq!(
            state.request(generation, 3, 0, 8, 1).unwrap_err().data,
            Some(Vec::new())
        );
        assert_eq!(
            state.request(generation - 1, 4, 0, 8, 1).unwrap_err().data,
            None
        );
    }
}
