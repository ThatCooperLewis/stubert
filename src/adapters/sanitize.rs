/// Sanitize a user-submitted filename for safe local storage.
///
/// Strips path components, replaces unsafe characters, and resolves
/// collisions by appending numeric suffixes.
pub fn sanitize_filename(name: &str, existing_files: &[String]) -> String {
    // Strip path components (both Unix `/` and Windows `\`)
    let basename = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");

    // Replace unsafe chars (keep alphanumeric, `.`, `-`, `_`)
    let sanitized: String = basename
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // Handle edge cases
    let sanitized = if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        "unnamed".to_string()
    } else {
        sanitized
    };

    // Collision resolution
    if !existing_files.contains(&sanitized) {
        return sanitized;
    }

    let (stem, ext) = match sanitized.rfind('.') {
        Some(pos) if pos > 0 => (&sanitized[..pos], Some(&sanitized[pos..])),
        _ => (sanitized.as_str(), None),
    };

    let mut counter = 1;
    loop {
        let candidate = match ext {
            Some(ext) => format!("{stem}-{counter}{ext}"),
            None => format!("{stem}-{counter}"),
        };
        if !existing_files.contains(&candidate) {
            return candidate;
        }
        counter += 1;
    }
}

#[cfg(test)]
mod test_sanitize_filename {
    use super::*;

    #[test]
    fn strips_unix_path_traversal() {
        assert_eq!(sanitize_filename("../../../etc/passwd", &[]), "passwd");
    }

    #[test]
    fn strips_windows_path() {
        assert_eq!(sanitize_filename(r"C:\Users\doc.pdf", &[]), "doc.pdf");
    }

    #[test]
    fn replaces_unsafe_characters() {
        assert_eq!(
            sanitize_filename("file name (1).pdf", &[]),
            "file_name__1_.pdf"
        );
    }

    #[test]
    fn collision_appends_numeric_suffix() {
        let existing = vec!["file.txt".to_string()];
        assert_eq!(sanitize_filename("file.txt", &existing), "file-1.txt");
    }

    #[test]
    fn multiple_collisions_increment() {
        let existing = vec!["file.txt".to_string(), "file-1.txt".to_string()];
        assert_eq!(sanitize_filename("file.txt", &existing), "file-2.txt");
    }

    #[test]
    fn handles_no_extension_collision() {
        let existing = vec!["README".to_string()];
        assert_eq!(sanitize_filename("README", &existing), "README-1");
    }

    #[test]
    fn empty_name_becomes_unnamed() {
        assert_eq!(sanitize_filename("", &[]), "unnamed");
    }

    #[test]
    fn mixed_path_separators() {
        assert_eq!(sanitize_filename("path/to\\file.txt", &[]), "file.txt");
    }

    #[test]
    fn dot_becomes_unnamed() {
        assert_eq!(sanitize_filename(".", &[]), "unnamed");
    }

    #[test]
    fn dotdot_becomes_unnamed() {
        assert_eq!(sanitize_filename("..", &[]), "unnamed");
    }
}
