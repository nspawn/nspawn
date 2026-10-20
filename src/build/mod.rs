//! Builds of Dockerfiles: the instructions read as the reference frontend reads them,
//! each step run in a root of its own and turned into a layer, the result installed as
//! a local image.

pub mod cache;
pub mod copy;
pub mod dispatch;
pub mod dockerfile;
pub mod exec;
pub mod ignore;
pub mod image;
pub mod layer;
pub mod plan;
pub mod root;
pub mod sandbox;
pub mod walk;
