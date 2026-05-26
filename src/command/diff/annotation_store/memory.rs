use super::{
    Annotation, AnnotationError, AnnotationId, AnnotationStore, Author, Capabilities, Comment,
    NewAnnotation,
};

const STORE_ID: &str = "local";

/// Session-local annotation store. Native IDs are sequential `u64`s rendered
/// as strings — stable only for the lifetime of this process.
pub struct InMemoryStore {
    annotations: Vec<Annotation>,
    next_id: u64,
}

impl InMemoryStore {
    pub fn new() -> Self {
        Self {
            annotations: Vec::new(),
            next_id: 0,
        }
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
        let id = AnnotationId {
            origin: STORE_ID.to_string(),
            native: self.next_id.to_string(),
        };
        self.next_id += 1;
        let root = Comment {
            author: Author::local(),
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
        if id.origin != STORE_ID {
            return Err(AnnotationError::WrongOrigin);
        }
        match self.annotations.iter_mut().find(|a| &a.id == id) {
            Some(ann) => {
                ann.root_mut().content = content;
                Ok(())
            }
            None => Err(AnnotationError::NotFound),
        }
    }

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError> {
        if id.origin != STORE_ID {
            return Err(AnnotationError::WrongOrigin);
        }
        let before = self.annotations.len();
        self.annotations.retain(|a| &a.id != id);
        if self.annotations.len() == before {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }
}
