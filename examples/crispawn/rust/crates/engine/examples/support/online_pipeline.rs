// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
//! Actual codec/hash/write/sync/concurrent-readback pipeline for all controls.
use super::*;

fn thread_cpu() -> f64 {
    let mut t = std::mem::MaybeUninit::<libc::timespec>::uninit();
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, t.as_mut_ptr()) },
        0
    );
    let t = unsafe { t.assume_init() };
    t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9
}

pub fn run(
    source: &Path,
    destination: &Path,
    resources: &Resources,
    name: &str,
    choice: &Choice,
    bytes: u64,
    adaptive: bool,
) -> AnyResult<serde_json::Value> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    event(json!({"event":"pipeline_start","sequence":sequence,"id":name,"heldout":true}));
    // Include source/cache setup and thread/pool startup for every variant.
    let start = Instant::now();
    let cpu_start = cpu_seconds();
    let source = File::open(source)?;
    let identity = crispawn_engine::io::identity(&source)?;
    drop_cache(&source, 0, 0)?;
    let half = identity.bytes / 2;
    let frame = if adaptive {
        16 * MIB
    } else {
        choice.plan.range_bytes
    };
    let ranges = if adaptive {
        two_ends(half, bytes, frame)?
    } else {
        read_ranges(half, bytes, frame, true)?
    };
    let path = destination.join(format!(
        "online-study-{}-{sequence}.data",
        std::process::id()
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let verify_file = File::open(&path)?;
    let output = Mutex::new(Output {
        file,
        end: 0,
        pending: vec![],
        sync_seconds: 0.0,
    });
    let control = Mutex::new(Controller::new(resources));
    let max_workers = if adaptive {
        control.lock().unwrap().max_workers
    } else {
        choice.plan.workers
    };
    let verifier_threads = if adaptive {
        resources.cpus
    } else {
        choice.verify_workers
    };
    // Identical hash CPU allocation in all three variants.
    let pool = CpuPool::new(resources.cpus)?;
    let next = AtomicUsize::new(0);
    let verified = AtomicU64::new(0);
    let overlap = AtomicU64::new(0);
    let saved_raw = AtomicU64::new(0);
    let copy_done = AtomicBool::new(false);
    let verify_active = AtomicU64::new(0);
    let verify_peak = AtomicU64::new(0);
    let copy_active = AtomicU64::new(0);
    let copy_peak = AtomicU64::new(0);
    let read_ns = AtomicU64::new(0);
    let writer_lock_ns = AtomicU64::new(0);
    let queue_ns = AtomicU64::new(0);
    let errors = Mutex::new(Vec::<String>::new());
    let (tx, rx) = mpsc::sync_channel::<Record>((resources.cpus as usize * 8).max(1));
    let rx = Arc::new(Mutex::new(rx));
    let profiles = Mutex::new(std::collections::BTreeMap::<String, u64>::new());
    std::thread::scope(|scope| {
        for worker in 0..verifier_threads {
            let rx = rx.clone();
            let control = &control;
            let copy_done = &copy_done;
            let vf = &verify_file;
            let pool = &pool;
            let verified = &verified;
            let errors = &errors;
            let active = &verify_active;
            let peak = &verify_peak;
            scope.spawn(move || loop {
                if adaptive && worker >= control.lock().unwrap().settings.verify_workers {
                    if copy_done.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                let record = rx.lock().unwrap().recv();
                let Ok(record) = record else { break };
                let n = active.fetch_add(1, Ordering::Relaxed) + 1;
                peak.fetch_max(n, Ordering::Relaxed);
                let read_bytes = u64::from_le_bytes(record.header[80..88].try_into().unwrap());
                match verify_record(vf, &record, pool, read_bytes) {
                    Ok(n) => {
                        verified.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(error) => errors.lock().unwrap().push(error.to_string()),
                }
                active.fetch_sub(1, Ordering::Relaxed);
            });
        }
        std::thread::scope(|writers| {
            for worker in 0..max_workers {
                let tx = tx.clone();
                let source = &source;
                let control = &control;
                let next = &next;
                let ranges = &ranges;
                let output = &output;
                let pool = &pool;
                let errors = &errors;
                let active = &copy_active;
                let peak = &copy_peak;
                let read_ns = &read_ns;
                let writer_lock_ns = &writer_lock_ns;
                let queue_ns = &queue_ns;
                let profiles = &profiles;
                let saved_raw = &saved_raw;
                writers.spawn(move || {
                    let result=(||->AnyResult<()> {
                        loop {
                            if next.load(Ordering::Acquire)>=ranges.len() {break;}
                            if adaptive && worker>=control.lock().unwrap().settings.workers {
                                std::thread::sleep(std::time::Duration::from_millis(2));continue;
                            }
                            let index=next.fetch_add(1,Ordering::AcqRel);
                            if index>=ranges.len() {break;}
                            let (offset,len)=ranges[index];let lane=usize::from(offset-half>=bytes/2);
                            let (requested,settings)=if adaptive {control.lock().unwrap().next_profile(lane)}
                            else {(Profile{codec:choice.plan.codec,level:choice.plan.compression_level},
                                Settings{epoch:0,read_bytes:choice.plan.read_bytes,workers:choice.plan.workers,verify_workers:choice.verify_workers})};
                            let n=active.fetch_add(1,Ordering::Relaxed)+1;peak.fetch_max(n,Ordering::Relaxed);
                            let mut raw=vec![0;len as usize];let reading=Instant::now();
                            read_at(source,&mut raw,offset,settings.read_bytes)?;
                            let read_seconds=reading.elapsed().as_secs_f64();
                            read_ns.fetch_add((read_seconds*1e9) as u64,Ordering::Relaxed);
                            let hash=pool.hash(&raw);let before=thread_cpu();
                            let encoded=codec::encode(raw,requested.codec,requested.level)?;
                            let encoder_cpu=thread_cpu()-before;
                            let level=if matches!(encoded.codec,Codec::Raw|Codec::Zero) {0} else {requested.level};
                            let stored=128+encoded.payload.len() as u64;
                            let mut header=[0u8;128];header[..8].copy_from_slice(b"PBSTUDY1");
                            header[8..16].copy_from_slice(&offset.to_le_bytes());header[16..24].copy_from_slice(&len.to_le_bytes());
                            header[24..32].copy_from_slice(&(encoded.payload.len() as u64).to_le_bytes());
                            header[32..64].copy_from_slice(&hash);header[64..68].copy_from_slice(&encoded.codec.id().to_le_bytes());
                            header[68..72].copy_from_slice(&level.to_le_bytes());header[72..76].copy_from_slice(&requested.codec.id().to_le_bytes());
                            header[76..80].copy_from_slice(&requested.level.to_le_bytes());header[80..88].copy_from_slice(&settings.read_bytes.to_le_bytes());
                            header[88..92].copy_from_slice(&settings.workers.to_le_bytes());header[92..96].copy_from_slice(&settings.verify_workers.to_le_bytes());
                            header[96..104].copy_from_slice(&settings.epoch.to_le_bytes());
                            let waiting=Instant::now();let mut out=output.lock().unwrap();
                            writer_lock_ns.fetch_add(waiting.elapsed().as_nanos() as u64,Ordering::Relaxed);
                            let writing=Instant::now();let record_offset=out.end;
                            out.file.write_all(&header)?;out.file.write_all(&encoded.payload)?;out.end+=stored;
                            out.pending.push(Record{offset:record_offset,payload:encoded.payload.len(),raw:len as usize,codec:encoded.codec,hash,header});
                            let batch=if out.pending.len()>=8 {
                                let syncing=Instant::now();out.file.sync_data()?;out.sync_seconds+=syncing.elapsed().as_secs_f64();
                                drop_cache(&out.file,0,out.end)?;Some(std::mem::take(&mut out.pending))
                            } else {None};
                            let writer_seconds=writing.elapsed().as_secs_f64();drop(out);
                            saved_raw.fetch_add(len,Ordering::Relaxed);
                            if let Some(batch)=batch {
                                let waiting=Instant::now();for record in batch {tx.send(record).map_err(|_|"verification queue closed")?;}
                                queue_ns.fetch_add(waiting.elapsed().as_nanos() as u64,Ordering::Relaxed);
                            }
                            profiles.lock().unwrap().entry(format!("{}:{}",requested.codec.name(),requested.level))
                                .and_modify(|n|*n+=len).or_insert(len);
                            if adaptive {control.lock().unwrap().observe(&FrameObservation{lane,requested,actual:encoded.codec,
                                raw:len,stored,encoder_cpu,read_seconds,read_bytes:settings.read_bytes,writer_seconds});}
                            event(json!({"event":"frame_saved","sequence":sequence,"variant":name,"lane":lane,
                                "source_offset_bytes":offset,"source_length_bytes":len,"stored_bytes":stored,
                                "requested_codec":requested.codec,"requested_level":requested.level,
                                "actual_codec":encoded.codec,"actual_level":level,"settings":settings,
                                "encoder_cpu_seconds":encoder_cpu,"read_seconds":read_seconds,
                                "writer_seconds":writer_seconds,"record_offset_bytes":record_offset}));
                            active.fetch_sub(1,Ordering::Relaxed);
                        }
                        Ok(())
                    })();
                    if let Err(error)=result {errors.lock().unwrap().push(error.to_string());}
                });
            }
        });
        overlap.store(verified.load(Ordering::Relaxed), Ordering::Relaxed);
        copy_done.store(true, Ordering::Release);
        let pending = {
            let mut out = output.lock().unwrap();
            let syncing = Instant::now();
            out.file.sync_data().unwrap();
            out.sync_seconds += syncing.elapsed().as_secs_f64();
            drop_cache(&out.file, 0, out.end).unwrap();
            std::mem::take(&mut out.pending)
        };
        for record in pending {
            tx.send(record).unwrap();
        }
        drop(tx);
    });
    if !errors.lock().unwrap().is_empty() {
        return Err(errors.lock().unwrap().join("; ").into());
    }
    if verified.load(Ordering::Relaxed) != bytes || saved_raw.load(Ordering::Relaxed) != bytes {
        return Err("incomplete online coverage/verification".into());
    }
    crispawn_engine::io::unchanged(&source, &identity)?;
    let (stored_bytes, sync_seconds) = {
        let out = output.lock().unwrap();
        (out.end, out.sync_seconds)
    };
    let result = json!({"seconds":start.elapsed().as_secs_f64(),"cpu_seconds":cpu_seconds()-cpu_start,
        "raw_bytes":bytes,"stored_bytes":stored_bytes,"verified_bytes":verified.load(Ordering::Relaxed),
        "copy_peak":copy_peak.load(Ordering::Relaxed),"verify_peak":verify_peak.load(Ordering::Relaxed),
        "verified_during_copy_bytes":overlap.load(Ordering::Relaxed),"profiles_raw_bytes":profiles.lock().unwrap().clone(),
        "writer_sync_seconds":sync_seconds,"read_stage_aggregate_seconds":read_ns.load(Ordering::Relaxed) as f64/1e9,
        "writer_lock_aggregate_seconds":writer_lock_ns.load(Ordering::Relaxed) as f64/1e9,
        "verification_queue_aggregate_seconds":queue_ns.load(Ordering::Relaxed) as f64/1e9,
        "settings_changes":control.lock().unwrap().changes,"adaptive":adaptive,"frame_bytes":frame,
        "pre_copy_probe_bytes":0,"hashes_are_private_study_record_hashes":true});
    drop(output);
    drop(verify_file);
    fs::remove_file(&path)?;
    event(
        json!({"event":"pipeline_finished","sequence":sequence,"id":name,"heldout":true,"measurement":result}),
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn productive_online_copy_and_both_controls_preserve_source_and_verify_all_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.img");
        let mut bytes = vec![0; 128 << 20];
        for (i, b) in bytes[..96 << 20].iter_mut().enumerate() {
            *b = (i.wrapping_mul(31).wrapping_add(i / 251) % 256) as u8;
        }
        fs::write(&source, &bytes).unwrap();
        let resources = Resources {
            cpus: 2,
            memory_bytes: 4 << 30,
            source_bps: 1e9,
            destination_write_bps: 1e8,
            destination_read_bps: 1e9,
        };
        let choice = Choice::new(
            &resources,
            &Cost {
                codec: Codec::Zstd,
                level: 3,
                frame_bytes: 8 * MIB,
                stored_ratio: 0.5,
                encode_cpu_per_byte: 1e-9,
                verify_cpu_per_byte: 1e-9,
            },
            MIB,
            2,
            1,
        )
        .unwrap();
        for adaptive in [false, true] {
            let result = run(
                &source,
                temp.path(),
                &resources,
                "test",
                &choice,
                64 * MIB,
                adaptive,
            )
            .unwrap();
            assert_eq!(result["verified_bytes"], 64 * MIB);
            assert_eq!(result["pre_copy_probe_bytes"], 0);
        }
        assert_eq!(fs::read(source).unwrap(), bytes);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
