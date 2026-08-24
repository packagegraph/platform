//! Shared RDF emission from the collector IR.
//!
//! The emitter reads `PackageIr` records and writes N-Triples according to
//! the current ontology contract. This is where ALL ontology-specific decisions
//! are made: URI construction, type assignments, property choices, inverse edges.

pub mod debian_ext;
pub mod rdf;
pub mod rpm_ext;
