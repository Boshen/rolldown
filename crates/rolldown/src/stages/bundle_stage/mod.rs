use crate::{
  error::BatchedResult,
  InputOptions,
  {
    chunk_graph::ChunkGraph,
    finalizer::FinalizerContext,
    options::{
      file_name_template::FileNameRenderOptions,
      output_options::{OutputOptions, SourceMapType},
    },
    plugin_driver::SharedPluginDriver,
    stages::link_stage::LinkStageOutput,
    utils::{finalize_normal_module, is_in_rust_test_mode, render_chunks::render_chunks},
  },
};
use rolldown_common::{ChunkKind, Output, OutputAsset, OutputChunk};
use rolldown_error::BuildError;
use rustc_hash::FxHashSet;

mod code_splitting;
mod compute_cross_chunk_links;

pub struct BundleStage<'a> {
  link_output: &'a mut LinkStageOutput,
  output_options: &'a OutputOptions,
  input_options: &'a InputOptions,
  plugin_driver: &'a SharedPluginDriver,
}

impl<'a> BundleStage<'a> {
  pub fn new(
    link_output: &'a mut LinkStageOutput,
    input_options: &'a InputOptions,
    output_options: &'a OutputOptions,
    plugin_driver: &'a SharedPluginDriver,
  ) -> Self {
    Self { link_output, output_options, input_options, plugin_driver }
  }

  #[tracing::instrument(skip_all)]
  pub async fn bundle(&mut self) -> BatchedResult<Vec<Output>> {
    use rayon::prelude::*;
    tracing::debug!("Start bundle stage");
    let mut chunk_graph = self.generate_chunks();

    self.generate_chunk_filenames(&mut chunk_graph);

    self.compute_cross_chunk_links(&mut chunk_graph);

    chunk_graph.chunks.iter_mut().par_bridge().for_each(|chunk| {
      chunk.de_conflict(self.link_output);
    });

    self
      .link_output
      .ast_table
      .iter_mut_enumerated()
      .par_bridge()
      .filter(|(id, _)| self.link_output.module_table.normal_modules[*id].is_included)
      .for_each(|(id, ast)| {
        let module = &self.link_output.module_table.normal_modules[id];
        let chunk_id = chunk_graph.module_to_chunk[module.id].unwrap();
        let chunk = &chunk_graph.chunks[chunk_id];
        let linking_info = &self.link_output.metas[module.id];
        finalize_normal_module(
          module,
          FinalizerContext {
            canonical_names: &chunk.canonical_names,
            id: module.id,
            symbols: &self.link_output.symbols,
            linking_info,
            module,
            modules: &self.link_output.module_table.normal_modules,
            linking_infos: &self.link_output.metas,
            runtime: &self.link_output.runtime,
            chunk_graph: &chunk_graph,
          },
          ast,
        );
      });

    let chunks = chunk_graph.chunks.iter().map(|c| {
      let ret =
        c.render(self.input_options, self.link_output, &chunk_graph, self.output_options).unwrap();
      (
        ret.code,
        ret.map,
        c.get_rendered_chunk_info(self.link_output, self.output_options, ret.rendered_modules),
      )
    });

    let mut assets = vec![];

    render_chunks(self.plugin_driver, chunks).await?.into_iter().try_for_each(
      |(mut content, map, rendered_chunk)| -> Result<(), BuildError> {
        if let Some(mut map) = map {
          match self.output_options.sourcemap {
            SourceMapType::File => {
              if let Some(map) = map.to_json() {
                assets.push(Output::Asset(Box::new(OutputAsset {
                  file_name: format!("{}.map", rendered_chunk.file_name),
                  source: map?,
                })));
              }
            }
            SourceMapType::Inline => {
              if let Some(map) = map.to_data_url() {
                content.push_str(&format!("\n//# sourceMappingURL={}", map?));
              }
            }
            SourceMapType::Hidden => {}
          }
        }
        assets.push(Output::Chunk(Box::new(OutputChunk {
          file_name: rendered_chunk.file_name,
          code: content,
          is_entry: rendered_chunk.is_entry,
          is_dynamic_entry: rendered_chunk.is_dynamic_entry,
          facade_module_id: rendered_chunk.facade_module_id,
          modules: rendered_chunk.modules,
          exports: rendered_chunk.exports,
          module_ids: rendered_chunk.module_ids,
        })));
        Ok(())
      },
    )?;

    Ok(assets)
  }

  fn generate_chunk_filenames(&self, chunk_graph: &mut ChunkGraph) {
    let mut used_chunk_names = FxHashSet::default();
    chunk_graph.chunks.iter_mut().for_each(|chunk| {
      let runtime_id = self.link_output.runtime.id();

      let file_name_tmp = chunk.file_name_template(self.output_options);
      let chunk_name =
        if is_in_rust_test_mode() && chunk.modules.first().copied() == Some(runtime_id) {
          "$runtime$".to_string()
        } else {
          chunk.name.clone().unwrap_or_else(|| {
            let module_id =
              if let ChunkKind::EntryPoint { module: entry_module_id, is_user_defined, .. } =
                &chunk.kind
              {
                debug_assert!(
                  !*is_user_defined,
                  "User-defined entry point should always have a name"
                );
                *entry_module_id
              } else {
                // TODO: we currently use the first executed module to calculate the chunk name for common chunks
                // This is not perfect, should investigate more to find a better solution
                chunk.modules.first().copied().unwrap()
              };
            let module = &self.link_output.module_table.normal_modules[module_id];
            module.resource_id.expect_file().unique(&self.input_options.cwd)
          })
        };

      let mut chunk_name = chunk_name;
      while used_chunk_names.contains(&chunk_name) {
        chunk_name = format!("{}-{}", chunk_name, used_chunk_names.len());
      }
      used_chunk_names.insert(chunk_name.clone());

      chunk.file_name =
        Some(file_name_tmp.render(&FileNameRenderOptions { name: Some(&chunk_name) }));
    });
  }
}
