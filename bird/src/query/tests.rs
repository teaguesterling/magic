//! Tests for the query parser.

use super::*;

#[test]
fn test_empty_query() {
    let q = parse_query("");
    assert!(q.is_match_all());
}

#[test]
fn test_simple_range() {
    let q = parse_query("~1");
    assert_eq!(q.range, Some(RangeSelector { start: 1, end: None }));
    assert!(q.source.is_none());
    assert!(q.path.is_none());
}

#[test]
fn test_bare_integer_range() {
    let q = parse_query("5");
    assert_eq!(q.range, Some(RangeSelector { start: 5, end: None }));
}

#[test]
fn test_bare_integer_range_large() {
    let q = parse_query("100");
    assert_eq!(q.range, Some(RangeSelector { start: 100, end: None }));
}

#[test]
fn test_range_span() {
    let q = parse_query("~5:~2");
    assert_eq!(
        q.range,
        Some(RangeSelector {
            start: 5,
            end: Some(2)
        })
    );
}

#[test]
fn test_range_span_no_second_tilde() {
    // ~5:2 is equivalent to ~5:~2
    let q = parse_query("~5:2");
    assert_eq!(
        q.range,
        Some(RangeSelector {
            start: 5,
            end: Some(2)
        })
    );
}

#[test]
fn test_source_type_only() {
    let q = parse_query("shell:");
    let source = q.source.unwrap();
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert!(source.host.is_none());
    assert!(source.client.is_none());
    assert!(source.session.is_none());
}

#[test]
fn test_source_type_client() {
    let q = parse_query("shell:zsh:");
    let source = q.source.unwrap();
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(source.client, Some("zsh".to_string()));
}

#[test]
fn test_source_host_type_client() {
    let q = parse_query("laptop:shell:zsh:");
    let source = q.source.unwrap();
    assert_eq!(source.host, Some("laptop".to_string()));
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(source.client, Some("zsh".to_string()));
}

#[test]
fn test_source_full() {
    let q = parse_query("laptop:shell:zsh:123:");
    let source = q.source.unwrap();
    assert_eq!(source.host, Some("laptop".to_string()));
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(source.client, Some("zsh".to_string()));
    assert_eq!(source.session, Some("123".to_string()));
}

#[test]
fn test_source_wildcards() {
    let q = parse_query("*:*:*:*:");
    assert!(q.is_all_sources());
    let source = q.source.unwrap();
    assert_eq!(source.host, Some("*".to_string()));
    assert_eq!(source.source_type, Some("*".to_string()));
    assert_eq!(source.client, Some("*".to_string()));
    assert_eq!(source.session, Some("*".to_string()));
}

#[test]
fn test_source_partial_wildcards() {
    let q = parse_query("*:shell:*:*:");
    let source = q.source.unwrap();
    assert_eq!(source.host, Some("*".to_string()));
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(source.client, Some("*".to_string()));
    assert_eq!(source.session, Some("*".to_string()));
}

#[test]
fn test_source_with_range() {
    let q = parse_query("shell:~5");
    let source = q.source.unwrap();
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(q.range, Some(RangeSelector { start: 5, end: None }));
}

#[test]
fn test_path_current_dir() {
    let q = parse_query(".");
    assert_eq!(q.path, Some(PathFilter::Current));
}

#[test]
fn test_path_current_dir_slash() {
    let q = parse_query("./");
    assert_eq!(q.path, Some(PathFilter::Current));
}

#[test]
fn test_path_relative() {
    let q = parse_query("./src/");
    assert_eq!(
        q.path,
        Some(PathFilter::Relative(std::path::PathBuf::from("./src/")))
    );
}

#[test]
fn test_path_parent() {
    let q = parse_query("../");
    assert_eq!(
        q.path,
        Some(PathFilter::Relative(std::path::PathBuf::from("../")))
    );
}

#[test]
fn test_path_home() {
    let q = parse_query("~/Projects/magic/");
    assert_eq!(
        q.path,
        Some(PathFilter::Home(std::path::PathBuf::from("Projects/magic/")))
    );
}

#[test]
fn test_path_absolute() {
    let q = parse_query("/tmp/");
    assert_eq!(
        q.path,
        Some(PathFilter::Absolute(std::path::PathBuf::from("/tmp/")))
    );
}

#[test]
fn test_path_with_range() {
    let q = parse_query(".~1");
    assert_eq!(q.path, Some(PathFilter::Current));
    assert_eq!(q.range, Some(RangeSelector { start: 1, end: None }));
}

#[test]
fn test_path_home_with_range() {
    let q = parse_query("~/Projects/~5");
    assert_eq!(
        q.path,
        Some(PathFilter::Home(std::path::PathBuf::from("Projects/")))
    );
    assert_eq!(q.range, Some(RangeSelector { start: 5, end: None }));
}

#[test]
fn test_cmd_regex() {
    let q = parse_query("%/make/");
    assert_eq!(q.filters.len(), 1);
    assert!(matches!(&q.filters[0], QueryComponent::CommandRegex(p) if p == "make"));
}

#[test]
fn test_cmd_regex_complex() {
    let q = parse_query("%/^cargo test/");
    assert_eq!(q.filters.len(), 1);
    assert!(matches!(&q.filters[0], QueryComponent::CommandRegex(p) if p == "^cargo test"));
}

#[test]
fn test_field_filter_exit_neq() {
    // Using <> (preferred, shell-friendly)
    let q = parse_query("%exit<>0");
    assert_eq!(q.filters.len(), 1);
    if let QueryComponent::FieldFilter(f) = &q.filters[0] {
        assert_eq!(f.field, "exit");
        assert_eq!(f.op, CompareOp::NotEq);
        assert_eq!(f.value, "0");
    } else {
        panic!("Expected FieldFilter");
    }
}

#[test]
fn test_field_filter_exit_neq_bang() {
    // Using != (also works but needs shell escaping)
    let q = parse_query("%exit!=0");
    assert_eq!(q.filters.len(), 1);
    if let QueryComponent::FieldFilter(f) = &q.filters[0] {
        assert_eq!(f.field, "exit");
        assert_eq!(f.op, CompareOp::NotEq);
        assert_eq!(f.value, "0");
    } else {
        panic!("Expected FieldFilter");
    }
}

#[test]
fn test_field_filter_duration() {
    let q = parse_query("%duration>5000");
    assert_eq!(q.filters.len(), 1);
    if let QueryComponent::FieldFilter(f) = &q.filters[0] {
        assert_eq!(f.field, "duration");
        assert_eq!(f.op, CompareOp::Gt);
        assert_eq!(f.value, "5000");
    } else {
        panic!("Expected FieldFilter");
    }
}

#[test]
fn test_field_filter_regex() {
    let q = parse_query("%cwd~=/duck_hunt/");
    assert_eq!(q.filters.len(), 1);
    if let QueryComponent::FieldFilter(f) = &q.filters[0] {
        assert_eq!(f.field, "cwd");
        assert_eq!(f.op, CompareOp::Regex);
        assert_eq!(f.value, "/duck_hunt/");
    } else {
        panic!("Expected FieldFilter");
    }
}

#[test]
fn test_tag_explicit() {
    let q = parse_query("%my-project");
    assert_eq!(q.filters.len(), 1);
    assert!(matches!(&q.filters[0], QueryComponent::Tag(t) if t == "my-project"));
}

#[test]
fn test_tag_bare() {
    let q = parse_query("my-project");
    assert_eq!(q.filters.len(), 1);
    assert!(matches!(&q.filters[0], QueryComponent::Tag(t) if t == "my-project"));
}

#[test]
fn test_filter_with_range() {
    let q = parse_query("%exit!=0~10");
    assert_eq!(q.filters.len(), 1);
    if let QueryComponent::FieldFilter(f) = &q.filters[0] {
        assert_eq!(f.field, "exit");
        assert_eq!(f.op, CompareOp::NotEq);
        assert_eq!(f.value, "0");
    } else {
        panic!("Expected FieldFilter");
    }
    assert_eq!(q.range, Some(RangeSelector { start: 10, end: None }));
}

#[test]
fn test_tag_with_range() {
    let q = parse_query("my-project~1");
    assert_eq!(q.filters.len(), 1);
    assert!(matches!(&q.filters[0], QueryComponent::Tag(t) if t == "my-project"));
    assert_eq!(q.range, Some(RangeSelector { start: 1, end: None }));
}

#[test]
fn test_complex_source_path_range() {
    let q = parse_query("shell:.~1");
    let source = q.source.unwrap();
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(q.path, Some(PathFilter::Current));
    assert_eq!(q.range, Some(RangeSelector { start: 1, end: None }));
}

#[test]
fn test_complex_source_filter_range() {
    let q = parse_query("shell:%exit!=0~5");
    let source = q.source.unwrap();
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(q.filters.len(), 1);
    assert_eq!(q.range, Some(RangeSelector { start: 5, end: None }));
}

#[test]
fn test_complex_all_sources_filter_range() {
    let q = parse_query("*:*:*:*:%/cargo test/~10");
    assert!(q.is_all_sources());
    assert_eq!(q.filters.len(), 1);
    assert!(matches!(&q.filters[0], QueryComponent::CommandRegex(p) if p == "cargo test"));
    assert_eq!(q.range, Some(RangeSelector { start: 10, end: None }));
}

#[test]
fn test_complex_full() {
    let q = parse_query("laptop:shell:zsh:%cwd~=/magic/%/make/~3");
    let source = q.source.unwrap();
    assert_eq!(source.host, Some("laptop".to_string()));
    assert_eq!(source.source_type, Some("shell".to_string()));
    assert_eq!(source.client, Some("zsh".to_string()));
    assert_eq!(q.filters.len(), 2);
    assert_eq!(q.range, Some(RangeSelector { start: 3, end: None }));
}

#[test]
fn test_compare_op_display() {
    assert_eq!(format!("{}", CompareOp::Eq), "=");
    assert_eq!(format!("{}", CompareOp::NotEq), "<>"); // Canonical form
    assert_eq!(format!("{}", CompareOp::Regex), "~=");
    assert_eq!(format!("{}", CompareOp::Gt), ">");
    assert_eq!(format!("{}", CompareOp::Lt), "<");
    assert_eq!(format!("{}", CompareOp::Gte), ">=");
    assert_eq!(format!("{}", CompareOp::Lte), "<=");
}
