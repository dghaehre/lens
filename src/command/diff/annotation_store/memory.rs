use super::{
    is_local_origin, Annotation, AnnotationError, AnnotationId, AnnotationStore, Author,
    Capabilities, Comment, NewAnnotation, NewReply,
};

const STORE_ID: &str = "local";

/// Session-local annotation store. Native IDs are sequential `u64`s rendered
/// as strings — stable only for the lifetime of this process.
pub struct InMemoryStore {
    annotations: Vec<Annotation>,
    next_annotation_id: u64,
    next_comment_id: u64,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            annotations: Vec::new(),
            next_annotation_id: 0,
            next_comment_id: 0,
        }
    }

    fn alloc_comment_id(&mut self) -> String {
        let id = self.next_comment_id;
        self.next_comment_id += 1;
        id.to_string()
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AnnotationStore for InMemoryStore {
    fn id(&self) -> &str {
        STORE_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn list(&self) -> Result<Vec<Annotation>, AnnotationError> {
        Ok(self.annotations.clone())
    }

    fn create(&mut self, draft: NewAnnotation) -> Result<Annotation, AnnotationError> {
        let native = self.next_annotation_id.to_string();
        self.next_annotation_id += 1;
        let id = AnnotationId {
            origin: draft.origin.clone(),
            native,
        };
        let root = Comment {
            native_id: Some(self.alloc_comment_id()),
            author: Author::for_origin(&draft.origin),
            origin: draft.origin,
            content: draft.content,
            created_at: draft.created_at,
        };
        let annotation = Annotation {
            id,
            filename: draft.filename,
            target: draft.target,
            comments: vec![root],
            resolved: false,
        };
        self.annotations.push(annotation.clone());
        Ok(annotation)
    }

    fn update(&mut self, id: &AnnotationId, content: String) -> Result<(), AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        match self
            .annotations
            .iter_mut()
            .find(|a| a.id.native == id.native)
        {
            Some(ann) => {
                ann.root_mut().content = content;
                Ok(())
            }
            None => Err(AnnotationError::NotFound),
        }
    }

    fn reply(&mut self, id: &AnnotationId, reply: NewReply) -> Result<Annotation, AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        let comment_id = self.alloc_comment_id();
        let ann = self
            .annotations
            .iter_mut()
            .find(|a| a.id.native == id.native)
            .ok_or(AnnotationError::NotFound)?;
        ann.comments.push(Comment {
            native_id: Some(comment_id),
            author: Author::for_origin(&reply.origin),
            origin: reply.origin,
            content: reply.content,
            created_at: reply.created_at,
        });
        Ok(ann.clone())
    }

    fn set_resolved(&mut self, id: &AnnotationId, resolved: bool) -> Result<(), AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        match self
            .annotations
            .iter_mut()
            .find(|a| a.id.native == id.native)
        {
            Some(ann) => {
                ann.resolved = resolved;
                Ok(())
            }
            None => Err(AnnotationError::NotFound),
        }
    }

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        let before = self.annotations.len();
        self.annotations.retain(|a| a.id.native != id.native);
        if self.annotations.len() == before {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }
}
