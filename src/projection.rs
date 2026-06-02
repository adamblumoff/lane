pub struct SourceProjection {
    mode: ProjectionMode,
    bytes: Vec<u8>,
}

impl SourceProjection {
    pub fn from_worktree(bytes: Vec<u8>) -> Self {
        let mode = ProjectionMode::for_bytes(&bytes);
        Self {
            bytes: mode.project(bytes),
            mode,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn materialize(&self, bytes: &[u8]) -> Vec<u8> {
        self.mode.materialize(bytes)
    }

    pub fn project_edit(&self, bytes: Vec<u8>) -> Vec<u8> {
        self.mode.project(bytes)
    }
}

#[derive(Clone, Copy)]
enum ProjectionMode {
    Raw,
    Text { newline: NewlineStyle },
}

impl ProjectionMode {
    fn for_bytes(bytes: &[u8]) -> Self {
        if is_text_projection_bytes(bytes) {
            Self::Text {
                newline: NewlineStyle::detect(bytes),
            }
        } else {
            Self::Raw
        }
    }

    fn project(self, bytes: Vec<u8>) -> Vec<u8> {
        match self {
            Self::Raw => bytes,
            Self::Text { .. } => canonicalize_text_bytes(bytes),
        }
    }

    fn materialize(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Raw => bytes.to_vec(),
            Self::Text { newline } => newline.materialize(canonicalize_text_bytes(bytes.to_vec())),
        }
    }
}

#[derive(Clone, Copy)]
enum NewlineStyle {
    Lf,
    Crlf,
    Cr,
}

impl NewlineStyle {
    fn detect(bytes: &[u8]) -> Self {
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => return Self::Lf,
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => return Self::Crlf,
                b'\r' => return Self::Cr,
                _ => index += 1,
            }
        }
        Self::Lf
    }

    fn materialize(self, bytes: Vec<u8>) -> Vec<u8> {
        match self {
            Self::Lf => bytes,
            Self::Crlf => expand_lf(bytes, b"\r\n"),
            Self::Cr => expand_lf(bytes, b"\r"),
        }
    }
}

fn is_text_projection_bytes(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn canonicalize_text_bytes(bytes: Vec<u8>) -> Vec<u8> {
    if !bytes.contains(&b'\r') {
        return bytes;
    }

    let mut projected = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' {
            projected.push(b'\n');
            if bytes.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
        } else {
            projected.push(bytes[index]);
        }
        index += 1;
    }
    projected
}

fn expand_lf(bytes: Vec<u8>, newline: &[u8]) -> Vec<u8> {
    let line_breaks = bytes.iter().filter(|byte| **byte == b'\n').count();
    if line_breaks == 0 || newline == b"\n" {
        return bytes;
    }

    let mut projected = Vec::with_capacity(bytes.len() + line_breaks * (newline.len() - 1));
    for byte in bytes {
        if byte == b'\n' {
            projected.extend_from_slice(newline);
        } else {
            projected.push(byte);
        }
    }
    projected
}
