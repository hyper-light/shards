use super::*;
use std::cell::Cell;
use std::rc::Rc;

fn id(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).unwrap()
}

#[test]
fn population_bounds_admission_and_refusals_return_the_record() {
    let mut pool = Pool::<String>::acquire(id(2)).unwrap();
    pool.prepare(id(1), "a".into()).unwrap();
    pool.prepare(id(2), "b".into()).unwrap();
    let (refusal, back) = pool.prepare(id(3), "c".into()).unwrap_err();
    assert_eq!((refusal, back.as_str()), (Refusal::ResourcesUnavailable, "c"));
    let (refusal, back) = pool.prepare(id(1), "again".into()).unwrap_err();
    assert_eq!((refusal, back.as_str()), (Refusal::Duplicate, "again"));
    assert_eq!(pool.counts().unwrap(), (2, 0, 0));
}

#[test]
fn retired_slots_refuse_duplicates_until_a_fence_reuses_them() {
    let mut pool = Pool::<u32>::acquire(id(2)).unwrap();
    pool.prepare(id(1), 10).unwrap();
    pool.prepare(id(2), 20).unwrap();
    assert_eq!(pool.retire(id(1)).unwrap(), 10);
    // A Retired slot is kept for duplicate refusal, so the pool is still full.
    assert_eq!(pool.prepare(id(1), 11).unwrap_err().0, Refusal::Duplicate);
    assert_eq!(
        pool.prepare(id(3), 30).unwrap_err().0,
        Refusal::ResourcesUnavailable
    );
    pool.fence(id(1)).unwrap();
    assert_eq!(pool.counts().unwrap(), (1, 0, 1));
    pool.prepare(id(3), 30).unwrap();
    // At or below the fence a Prepare is a duplicate even with no slot left.
    assert_eq!(
        pool.prepare(id(1), 12).unwrap_err().0,
        Refusal::Duplicate,
        "a fenced ID was admitted again"
    );
}

#[test]
fn live_records_survive_fences_and_keep_routing() {
    let mut pool = Pool::<u32>::acquire(id(3)).unwrap();
    pool.prepare(id(1), 1).unwrap();
    pool.prepare(id(2), 2).unwrap();
    pool.retire(id(2)).unwrap();
    pool.fence(id(2)).unwrap();
    assert_eq!(pool.counts().unwrap(), (1, 0, 2), "a fence freed a Live record");
    // Late cleanup for a Live ID at or below the fence still reaches it.
    *pool.get_mut(id(1)).unwrap() += 1;
    assert_eq!(*pool.get_mut(id(1)).unwrap(), 2);
    assert_eq!(pool.prepare(id(1), 9).unwrap_err().0, Refusal::Duplicate);
    assert_eq!(pool.get_mut(id(2)).unwrap_err(), Refusal::Identity);
    assert_eq!(pool.retire(id(2)).unwrap_err(), Refusal::Identity);
}

#[test]
fn fences_are_monotone_and_repeatable() {
    let mut pool = Pool::<u8>::acquire(id(1)).unwrap();
    pool.fence(id(5)).unwrap();
    pool.fence(id(5)).unwrap();
    assert_eq!(pool.fence(id(4)).unwrap_err(), Refusal::State);
    assert_eq!(pool.fenced_through(), 5);
}

#[test]
fn churn_around_a_stuck_operation_never_exceeds_the_population() {
    let mut pool = Pool::<u64>::acquire(id(4)).unwrap();
    // Operation 1 never discharges: it keeps one slot and nothing else stalls.
    pool.prepare(id(1), 1).unwrap();
    let mut next = 2u64;
    for _ in 0..1000 {
        loop {
            let admitted = pool.prepare(id(next), next).is_ok();
            // A refused ID is burned by the issuer, as an unsent one would be.
            next += 1;
            if !admitted {
                break;
            }
        }
        let done: Vec<u64> = pool
            .live_mut()
            .unwrap()
            .map(|(held, _)| held.get())
            .filter(|&held| held != 1)
            .collect();
        for held in done {
            pool.retire(id(held)).unwrap();
        }
        pool.fence(id(next - 1)).unwrap();
        assert_eq!(pool.counts().unwrap(), (1, 0, 3));
    }
    assert_eq!(*pool.get_mut(id(1)).unwrap(), 1);
}

#[derive(Debug)]
struct Counted(Rc<Cell<u32>>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn records_drop_exactly_once_and_release_refuses_while_live() {
    let drops = Rc::new(Cell::new(0));
    let mut pool = Pool::<Counted>::acquire(id(2)).unwrap();
    pool.prepare(id(1), Counted(drops.clone())).unwrap();
    pool.prepare(id(2), Counted(drops.clone())).unwrap();
    drop(pool.retire(id(2)).unwrap());
    assert_eq!(drops.get(), 1);
    let pool = match pool.release() {
        Err(Release::Live(pool)) => pool,
        other => panic!("released with a Live record: {other:?}"),
    };
    drop(pool);
    assert_eq!(drops.get(), 2);
}

#[test]
fn an_empty_pool_releases_its_mapping() {
    let mut pool = Pool::<u8>::acquire(id(3)).unwrap();
    pool.prepare(id(1), 1).unwrap();
    pool.retire(id(1)).unwrap();
    pool.release().unwrap();
}

#[test]
fn an_unrepresentable_population_is_refused_before_mapping() {
    assert!(matches!(
        Pool::<[u8; 4096]>::acquire(id(u64::MAX)),
        Err(Failure::Layout)
    ));
}
