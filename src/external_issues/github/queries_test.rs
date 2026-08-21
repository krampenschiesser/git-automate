//! Generic schema validation test for GitHub GraphQL queries.
//!
//! Every `*.graphql` file in `queries/` (excluding `schema.graphql` itself)
//! is automatically discovered and validated against the GitHub GraphQL schema
//! (`schema.graphql`) using [`apollo_compiler`].
//!
//! New query files are picked up with **zero** manual registration — just drop
//! a `.graphql` file in `queries/` and `cargo test` will validate it.
//!
//! This closes the gap noted in `AGENTS.md`:
//! > "No runtime query validation: queries are embedded at compile time;
//! > syntax errors only surface at runtime when GitHub returns a 400"

#[cfg(test)]
mod tests {
    use apollo_compiler::validation::Valid;
    use apollo_compiler::{ExecutableDocument, Schema};
    use std::path::PathBuf;

    // ─── T1: Schema discovery & parsing ──────────────────────────

    /// Absolute path to the embedded GitHub GraphQL schema (static SDL dump).
    fn schema_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/external_issues/github/schema.graphql")
    }

    /// Load and validate the GitHub GraphQL schema once.
    ///
    /// `schema.graphql` is a 64k-line SDL dump of the GitHub API schema.
    /// Parsing it here guarantees that every query we ship is checked against
    /// the real schema — catching typos in field names, arguments, and types
    /// long before they hit the wire.
    fn github_schema() -> Valid<Schema> {
        let sdl =
            std::fs::read_to_string(schema_path()).expect("schema.graphql should be readable");

        Schema::parse_and_validate(&sdl, "schema.graphql")
            .expect("schema.graphql should be valid GraphQL SDL")
    }

    // ─── T2: Query file auto-discovery ──────────────────────────

    /// Auto-discover all query `.graphql` files in the `queries/` directory.
    ///
    /// `schema.graphql` itself is **excluded** — it is the schema being
    /// validated against, not a query document.
    ///
    /// Files are discovered by scanning the directory at test time, so adding
    /// a new `*.graphql` file to `queries/` automatically includes it in the
    /// test with no code changes.
    fn discover_query_files() -> Vec<PathBuf> {
        let queries_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/external_issues/github/queries");

        let mut files: Vec<PathBuf> = std::fs::read_dir(&queries_dir)
            .expect("queries/ directory should exist")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                // Only .graphql files, excluding the schema itself
                p.extension().and_then(|e| e.to_str()) == Some("graphql")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n != "schema.graphql")
                        .unwrap_or(true)
            })
            .collect();

        // Sort for deterministic output ordering
        files.sort();
        files
    }

    // ─── T3: Full validation ────────────────────────────────────

    /// Validate every `.graphql` query file against the GitHub schema.
    ///
    /// For each file, [`ExecutableDocument::parse_and_validate`] checks:
    /// - Syntax is valid GraphQL
    /// - All referenced types, fields, and arguments exist in the schema
    /// - Inline fragments (`... on Type`) target valid types
    /// - Variable definitions have valid types
    /// - Selection sets are valid for their parent types (objects, interfaces,
    ///   unions, and scalars)
    ///
    /// All failures are collected and reported together so you can fix
    /// multiple issues in one pass rather than iterating one-at-a-time.
    #[test]
    fn all_graphql_queries_validate_against_github_schema() {
        let schema = github_schema();
        let files = discover_query_files();

        assert!(
            !files.is_empty(),
            "No .graphql query files found in queries/ directory — \
             is the test running from the project root?"
        );

        let mut failures: Vec<String> = Vec::new();

        for file in &files {
            let content = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("Failed to read {}: {}", file.display(), e));

            // The file path is used by apollo_compiler for error messages and
            // for resolving any file-relative references.
            let file_str = file
                .to_str()
                .unwrap_or_else(|| panic!("Non-UTF8 path: {}", file.display()));

            match ExecutableDocument::parse_and_validate(&schema, &content, file_str) {
                Ok(_) => {}
                Err(err) => {
                    failures.push(format!("{}:\n{:#?}", file.display(), err));
                }
            }
        }

        if !failures.is_empty() {
            panic!(
                "GraphQL schema validation failed for {} file(s):\n\n{}\n\
                 \nTotal: {} failure(s) across {} file(s)",
                failures.len(),
                failures.join("\n"),
                failures.len(),
                files.len()
            );
        }
    }

    // ─── T4: Each query file produces exactly one operation ─────
    // (Guards against accidentally embedding two operations in one file,
    //  which GitHub rejects when you send a single-operation query string.)

    /// Ensure each `.graphql` file contains exactly one operation definition
    /// (either a single query, mutation, or subscription).
    ///
    /// GitHub's GraphQL API accepts a single operation per request. Having
    /// multiple operations in one file requires a `operationName` parameter
    /// to disambiguate, which this codebase does not use.
    #[test]
    fn each_query_file_has_exactly_one_operation() {
        let files = discover_query_files();
        let schema = github_schema();

        let mut failures: Vec<String> = Vec::new();

        for file in &files {
            let content = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("Failed to read {}: {}", file.display(), e));

            let file_str = file
                .to_str()
                .unwrap_or_else(|| panic!("Non-UTF8 path: {}", file.display()));

            let doc = match ExecutableDocument::parse_and_validate(&schema, &content, file_str) {
                Ok(doc) => doc,
                Err(_) => continue, // Already reported by T3
            };

            let operation_count = doc.operations.len();
            if operation_count != 1 {
                failures.push(format!(
                    "{}: expected exactly 1 operation, found {}",
                    file.display(),
                    operation_count
                ));
            }
        }

        if !failures.is_empty() {
            panic!(
                "Operation count validation failed:\n\n{}\n\
                 \nTotal: {} failure(s)",
                failures.join("\n"),
                failures.len()
            );
        }
    }
}
