// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use derive_builder::Builder;

use gitlab::api::{common::NameOrId, endpoint_prelude::*};

/// Query for jobs within a pipeline.
#[derive(Debug, Builder)]
pub struct PipelineBridges<'a> {
    /// The project to query for the pipeline.
    #[builder(setter(into))]
    project: NameOrId<'a>,
    /// The ID of the pipeline.
    pipeline: u64,
}

impl<'a> PipelineBridges<'a> {
    /// Create a builder for the endpoint.
    pub fn builder() -> PipelineBridgesBuilder<'a> {
        PipelineBridgesBuilder::default()
    }
}

impl<'a> Endpoint for PipelineBridges<'a> {
    fn method(&self) -> Method {
        Method::GET
    }

    fn endpoint(&self) -> Cow<'static, str> {
        format!(
            "projects/{}/pipelines/{}/bridges",
            self.project, self.pipeline
        )
        .into()
    }

    fn parameters(&self) -> QueryParams {
        let params = QueryParams::default();
        params
    }
}

impl<'a> Pageable for PipelineBridges<'a> {}
