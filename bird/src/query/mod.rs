//! Query micro-language parser for cross-client queries.
//!
//! # Syntax Overview
//!
//! Full pattern: `[source][path][filters][range]`
//!
//! - **Source selectors**: `host:type:client:session:`
//! - **Path filters**: `.`, `~/`, `/path/`
//! - **Command regex**: `%/pattern/`
//! - **Field filters**: `%field<op>value`
//! - **Tags**: `%tag-name` or bare word
//! - **Range**: `~N` or `~N:~M`

mod parser;

pub use parser::{
    parse_query, CompareOp, FieldFilter, PathFilter, Query, QueryComponent, RangeSelector,
    SourceSelector,
};

#[cfg(test)]
mod tests;
