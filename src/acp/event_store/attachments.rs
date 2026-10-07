//! Prompt attachment blobs, stored beside the event that references them.

use super::EventStore;
use crate::daemon::PromptAttachmentKind;
use crate::events;

/// A decoded attachment ready to persist; the storage side of `PromptAttachmentUpload`.
pub struct AttachmentBlob {
    pub id: String,
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    pub name: Option<String>,
    pub data: Vec<u8>,
}

impl EventStore {
    /// Persist a blob keyed to its `UserPromptSent` seq, so retention and
    /// session delete drop it with that event.
    pub fn record_attachment(&self, session_id: &str, seq: u64, blob: &AttachmentBlob) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        events::insert_attachment(
            &self.conn(),
            &self.schema,
            session_id,
            seq,
            &blob.id,
            blob.kind.as_str(),
            &blob.mime_type,
            blob.name.as_deref(),
            &blob.data,
            now_ms,
        )
    }

    pub fn delete_attachments_for_seq(&self, session_id: &str, seq: u64) {
        events::delete_attachments_for_seq(&self.conn(), &self.schema, session_id, seq);
    }

    /// One attachment's MIME type and bytes.
    pub fn load_attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Option<(String, Vec<u8>)> {
        events::load_attachment(&self.conn(), &self.schema, session_id, attachment_id)
    }

    /// Buffer a blob for a queued prompt, keyed by the prompt's `ref_id`
    /// until it has a `UserPromptSent` seq.
    pub fn record_pending_attachment(
        &self,
        session_id: &str,
        ref_id: &str,
        blob: &AttachmentBlob,
    ) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        events::insert_pending_attachment(
            &self.conn(),
            &self.schema,
            session_id,
            ref_id,
            &blob.id,
            blob.kind.as_str(),
            &blob.mime_type,
            blob.name.as_deref(),
            &blob.data,
            now_ms,
        )
    }

    pub fn load_pending_attachments_for_ref(
        &self,
        session_id: &str,
        ref_id: &str,
    ) -> Vec<AttachmentBlob> {
        events::load_pending_attachments_for_ref(&self.conn(), &self.schema, session_id, ref_id)
            .into_iter()
            .filter_map(|(id, kind, mime_type, name, data)| {
                Some(AttachmentBlob {
                    id,
                    kind: PromptAttachmentKind::from_tag(&kind)?,
                    mime_type,
                    name,
                    data,
                })
            })
            .collect()
    }

    pub fn delete_pending_attachments_for_ref(&self, session_id: &str, ref_id: &str) {
        events::delete_pending_attachments_for_ref(&self.conn(), &self.schema, session_id, ref_id);
    }

    /// Bytes of queued attachments buffered for a session, for the enqueue cap.
    pub fn pending_attachment_bytes(&self, session_id: &str) -> u64 {
        events::pending_attachment_bytes_for_session(&self.conn(), &self.schema, session_id)
    }

    /// Prune queued blobs older than `max_age`, so a prompt queued against a
    /// session that never idles again cannot buffer bytes forever.
    pub fn prune_pending_attachments_older_than(&self, max_age: std::time::Duration) -> usize {
        let cutoff_ms = chrono::Utc::now().timestamp_millis() - (max_age.as_millis() as i64).max(0);
        events::prune_pending_attachments_older_than(&self.conn(), &self.schema, cutoff_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::acp::state::Event;

    fn img_blob(id: &str) -> AttachmentBlob {
        AttachmentBlob {
            id: id.to_string(),
            kind: PromptAttachmentKind::Image,
            mime_type: "image/png".into(),
            name: Some("shot.png".into()),
            data: vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3],
        }
    }

    fn record_prompt_with_attachment(store: &EventStore, session_id: &str, id: &str) {
        let prompt = Event::UserPromptSent {
            prompt_id: None,
            text: "look at this".into(),
            attachments: vec![crate::daemon::PromptAttachmentRef {
                id: id.to_string(),
                kind: PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                name: Some("shot.png".into()),
                size: 7,
            }],
            synthesized: false,
        };
        store.record(session_id, 1, &prompt).unwrap();
        assert!(store.record_attachment(session_id, 1, &img_blob(id)));
    }

    #[test]
    fn attachments_load_per_session_and_leave_with_their_prompt() {
        let (_tmp, store) = open_store(1000);
        record_prompt_with_attachment(&store, "s-1", "a1");
        record_prompt_with_attachment(&store, "s-2", "b1");
        let (mime, bytes) = store.load_attachment("s-1", "a1").expect("blob present");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, vec![0x89, 0x50, 0x4E, 0x47, 1, 2, 3]);
        assert!(
            store.load_attachment("s-2", "a1").is_none(),
            "scoped per session"
        );
        assert!(store.load_attachment("s-1", "nope").is_none());

        store.delete_session("s-1");
        assert!(store.load_attachment("s-1", "a1").is_none());
        assert!(
            store.load_attachment("s-2", "b1").is_some(),
            "sibling untouched"
        );

        let (_tmp, store) = open_store(3);
        record_prompt_with_attachment(&store, "s-1", "a1");
        record_from(&store, "s-1", 2, (2..=20).map(|_| Event::ThinkingStarted));
        assert!(
            store.load_attachment("s-1", "a1").is_none(),
            "attachment blob outlived its pruned prompt event"
        );
    }
}
