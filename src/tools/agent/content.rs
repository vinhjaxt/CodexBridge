use std::{fs::File, io::Read, path::Path};

use crate::error::{AppError, Result as AppResult};

pub(super) fn read_text_bounded(path: &Path, maximum: usize) -> AppResult<String> {
    if maximum == 0 {
        return Err(AppError::new(
            "RESOURCE_LIMIT_EXCEEDED",
            "agent content exceeds read limit",
        ));
    }

    let file = File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::new("FILE_NOT_FOUND", error.to_string())
        } else {
            AppError::new("PROCESS_FAILED", error.to_string())
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    if !metadata.is_file() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "agent content path is not a regular file",
        ));
    }
    if metadata.len() > maximum as u64 {
        return Err(AppError::new(
            "RESOURCE_LIMIT_EXCEEDED",
            "agent content exceeds read limit",
        ));
    }

    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(maximum));
    file.take(maximum.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    if bytes.len() > maximum {
        return Err(AppError::new(
            "RESOURCE_LIMIT_EXCEEDED",
            "agent content exceeds read limit",
        ));
    }

    String::from_utf8(bytes)
        .map_err(|error| AppError::new("PROCESS_FAILED", format!("UTF-8 read failed: {error}")))
}

pub(super) fn read_text_prefix_bounded(path: &Path, maximum: usize) -> AppResult<(String, bool)> {
    if maximum == 0 {
        return Ok((String::new(), true));
    }
    let file = File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::new("FILE_NOT_FOUND", error.to_string())
        } else {
            AppError::new("PROCESS_FAILED", error.to_string())
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    if !metadata.is_file() {
        return Err(AppError::new(
            "INVALID_INPUT",
            "agent content path is not a regular file",
        ));
    }
    let mut bytes = Vec::with_capacity(maximum.saturating_add(3));
    file.take(maximum.saturating_add(3) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::new("PROCESS_FAILED", error.to_string()))?;
    let desired = maximum.min(bytes.len());
    let shown = match std::str::from_utf8(&bytes[..desired]) {
        Ok(_) => desired,
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(error) => {
            return Err(AppError::new(
                "PROCESS_FAILED",
                format!("UTF-8 read failed: {error}"),
            ));
        }
    };
    let content = String::from_utf8(bytes[..shown].to_vec())
        .map_err(|error| AppError::new("PROCESS_FAILED", format!("UTF-8 read failed: {error}")))?;
    Ok((content, metadata.len() > shown as u64))
}

#[cfg(test)]
pub(crate) fn utf8_prefix_bounded(content: &str, maximum: usize) -> (String, bool) {
    if content.len() <= maximum {
        return (content.to_owned(), false);
    }
    let mut end = maximum.min(content.len());
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    (content[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::utf8_prefix_bounded;

    #[test]
    fn utf8_prefix_budget_counts_bytes_not_characters() {
        let source = "🙂".repeat(20);
        let (shown, truncated) = utf8_prefix_bounded(&source, 17);
        assert!(truncated);
        assert!(shown.len() <= 17);
        assert_eq!(shown, "🙂".repeat(4));
    }
}
