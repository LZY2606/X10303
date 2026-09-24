use lsm_audit::domain::case::*;
use lsm_audit::domain::engine::*;
use lsm_audit::domain::model::*;

#[test]
fn smoke_run_case() {
    let pack = case_pack();
    let (engine, reports) = run_case(&pack);
    for r in &reports {
        assert!(r.ok, "step {} failed: {:?} {}", r.ordinal, r.error_class, r.title);
    }
    let st = &engine.state;
    let c1 = st.compactions.iter().find(|c| c.job_name == "C1").unwrap();
    assert_eq!(c1.status, CompactionStatus::Installed);
    assert!(c1.outputs.len() >= 3, "C1 outputs = {}", c1.outputs.len());
    for w in c1.outputs.windows(2) {
        assert!(w[0].largest < w[1].smallest, "non-overlap outputs");
    }
    let c2 = st.compactions.iter().find(|c| c.job_name == "C2").unwrap();
    assert_eq!(c2.status, CompactionStatus::Installed);
    assert_eq!(c2.outputs[0].level, 2);
    assert!(!c2.range_retention.is_empty());
    println!("C1 outputs: {:?}", c1.outputs.iter().map(|o| format!("[{},{}]", o.smallest, o.largest)).collect::<Vec<_>>());
    println!("C2 outputs: {:?}", c2.outputs.iter().map(|o| format!("[{},{}]L{}", o.smallest, o.largest, o.level)).collect::<Vec<_>>());
    println!("files: {:?}", st.files.iter().map(|f| format!("{} L{} [{}{}]", f.id.get(0..10).unwrap_or(&f.id), f.level, f.smallest, f.largest)).collect::<Vec<_>>());
}

#[test]
fn debug_c1_detail() {
    let pack = case_pack();
    let (engine, _) = run_case(&pack);
    let st = &engine.state;
    let c1 = st.compactions.iter().find(|c| c.job_name == "C1").unwrap();
    for o in &c1.outputs {
        println!("OUT [{},{}] n={}", o.smallest, o.largest, o.entry_count);
        for e in &o.entries {
            println!("   {} seq={} op={:?}", e.key, e.seq, e.op);
        }
        for f in &o.range_fragments {
            println!("   FRAG {} [{},{}) seq={}", f.name, f.begin, f.end_exclusive, f.seq);
        }
    }
    for o in &c1.outputs {
        for d in &o.drops {
            println!("DROP {} seq={} {}", d.key, d.seq, d.reason_code);
        }
    }
    let c2 = st.compactions.iter().find(|c| c.job_name == "C2").unwrap();
    for n in &c2.range_retention {
        println!("C2KEEP {} {}", n.reason_code, n.detail);
    }
}
