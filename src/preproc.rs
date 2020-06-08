use crate::adapters::*;
use crate::args::RgaConfig;
use crate::matching::*;
use crate::CachingWriter;
use anyhow::*;
use log::*;
use path_clean::PathClean;
use std::convert::TryInto;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct PreprocConfig<'a> {
    pub cache: Option<Arc<RwLock<dyn crate::preproc_cache::PreprocCache>>>,
    pub args: &'a RgaConfig,
}
/**
 * preprocess a file as defined in `ai`.
 *
 * If a cache is passed, read/write to it.
 *
 */
pub fn rga_preproc(ai: AdaptInfo) -> Result<()> {
    let AdaptInfo {
        filepath_hint,
        is_real_file,
        inp,
        oup,
        line_prefix,
        config,
        archive_recursion_depth,
        ..
    } = ai;
    let PreprocConfig { mut cache, args } = config;
    let adapters = adapter_matcher(
        get_adapters_filtered(args.custom_adapters.clone(), &args.adapters)?,
        args.accurate,
    )?;
    let filename = filepath_hint
        .file_name()
        .ok_or_else(|| format_err!("Empty filename"))?;
    debug!("depth: {}", archive_recursion_depth);
    if archive_recursion_depth >= args.max_archive_recursion.0 {
        writeln!(oup, "{}[rga: max archive recursion reached]", line_prefix)?;
        return Ok(());
    }

    debug!("path_hint: {:?}", filepath_hint);

    // todo: figure out when using a bufreader is a good idea and when it is not
    // seems to be good for File::open() reads, but not sure about within archives (tar, zip)
    let inp = &mut BufReader::with_capacity(1 << 13, inp);

    let mimetype = if args.accurate {
        let buf = inp.fill_buf()?; // fill but do not consume!
        let mimetype = tree_magic::from_u8(buf);
        debug!("mimetype: {:?}", mimetype);
        Some(mimetype)
    } else {
        None
    };
    let adapter = adapters(FileMeta {
        mimetype,
        lossy_filename: filename.to_string_lossy().to_string(),
    });
    match adapter {
        Some((adapter, detection_reason)) => {
            let meta = adapter.metadata();
            debug!(
                "chose adapter '{}' because of matcher {:?}",
                &meta.name, &detection_reason
            );
            eprintln!("adapter: {}", &meta.name);
            let db_name = format!("{}.v{}", meta.name, meta.version);
            if let Some(cache) = cache.as_mut() {
                let cache_key: Vec<u8> = {
                    let clean_path = filepath_hint.to_owned().clean();
                    let meta = std::fs::metadata(&filepath_hint)?;

                    if adapter.metadata().recurses {
                        let key = (
                            clean_path,
                            meta.modified().expect("weird OS that can't into mtime"),
                            &args.adapters[..],
                        );
                        debug!("cache key: {:?}", key);
                        bincode::serialize(&key).expect("could not serialize path")
                    // key in the cache database
                    } else {
                        let key = (
                            clean_path,
                            meta.modified().expect("weird OS that can't into mtime"),
                        );
                        debug!("cache key: {:?}", key);
                        bincode::serialize(&key).expect("could not serialize path")
                        // key in the cache database
                    }
                };
                cache.write().unwrap().get_or_run(
                    &db_name,
                    &cache_key,
                    Box::new(|| -> Result<Option<Vec<u8>>> {
                        // wrapping BufWriter here gives ~10% perf boost
                        let mut compbuf = BufWriter::new(CachingWriter::new(
                            oup,
                            args.cache_max_blob_len.0.try_into().unwrap(),
                            args.cache_compression_level.0.try_into().unwrap(),
                        )?);
                        debug!("adapting...");
                        adapter
                            .adapt(
                                AdaptInfo {
                                    line_prefix,
                                    filepath_hint,
                                    is_real_file,
                                    inp,
                                    oup: &mut compbuf,
                                    archive_recursion_depth,
                                    config: PreprocConfig { cache: None, args },
                                },
                                &detection_reason,
                            )
                            .with_context(|| {
                                format!(
                                    "adapting {} via {} failed",
                                    filepath_hint.to_string_lossy(),
                                    meta.name
                                )
                            })?;
                        let compressed = compbuf
                            .into_inner()
                            .map_err(|_| "could not finish zstd") // can't use with_context here
                            .unwrap()
                            .finish()?;
                        if let Some(cached) = compressed {
                            debug!("compressed len: {}", cached.len());
                            Ok(Some(cached))
                        } else {
                            Ok(None)
                        }
                    }),
                    Box::new(|cached| {
                        let stdouti = std::io::stdout();
                        zstd::stream::copy_decode(cached, stdouti.lock())?;
                        Ok(())
                    }),
                )?;
                Ok(())
            } else {
                // couldn't open cache
                debug!("adapting...");
                adapter
                    .adapt(
                        AdaptInfo {
                            line_prefix,
                            filepath_hint,
                            is_real_file,
                            inp,
                            oup,
                            archive_recursion_depth,
                            config: PreprocConfig { cache: None, args },
                        },
                        &detection_reason,
                    )
                    .with_context(|| {
                        format!(
                            "adapting {} via {} without caching failed",
                            filepath_hint.to_string_lossy(),
                            meta.name
                        )
                    })?;
                Ok(())
            }
        }
        None => {
            // allow passthrough if the file is in an archive or accurate matching is enabled
            // otherwise it should have been filtered out by rg pre-glob since rg can handle those better than us
            let allow_cat = !is_real_file || args.accurate;
            if allow_cat {
                spawning::postproc_line_prefix(line_prefix, inp, oup)?;
                Ok(())
            } else {
                Err(format_err!(
                    "No adapter found for file {:?}, passthrough disabled.",
                    filename
                ))
            }
        }
    }
}
