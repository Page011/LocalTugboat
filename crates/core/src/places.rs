//! Place routing (used by the phone at capture, M4; implemented and tested now). Owner: logic.
//! Pure: no DB, no clock.
//!
//! PLAN Places:
//! P1 A place = name, centre, radius (default 150 m), target folder.
//! P2 A fix is usable if it is under 2 minutes old and accurate to 100 m or better.
//! P3 Overlapping places: the smallest radius wins. Tie (equal radius): lowest PlaceId bytes,
//!    so the result is deterministic.
//! P4 Places exist (at least one not deleted) but the location is unknown or unusable ->
//!    Unsorted ("No location" chip). Never a short-retention folder by accident.
//! P5 No live places, or no place contains the fix -> Default (caller uses the chip's folder,
//!    Inbox by default).
//! P6 A place whose target folder is deleted or unknown is skipped (as if it did not exist
//!    for P3), so a photo never routes to a deleted folder.
//! Containment: great-circle (haversine, R = 6_371_008.8 m) distance <= radius_m.

use crate::model::*;

pub const DEFAULT_RADIUS_M: f64 = 150.0;
pub const MAX_FIX_AGE_MS: i64 = 2 * 60 * 1000;
pub const MAX_FIX_ACCURACY_M: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
    pub age_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Place { place: PlaceId, folder: FolderId },
    Unsorted,
    Default,
}

/// Mean Earth radius used by `distance_m`.
const EARTH_RADIUS_M: f64 = 6_371_008.8;

fn fix_usable(fix: &Fix) -> bool {
    fix.lat.is_finite()
        && fix.lon.is_finite()
        && (-90.0..=90.0).contains(&fix.lat)
        && (-180.0..=180.0).contains(&fix.lon)
        && fix.accuracy_m.is_finite()
        && fix.accuracy_m >= 0.0
        && fix.accuracy_m <= MAX_FIX_ACCURACY_M
        // Under 2 minutes old; a fix from the future is a broken clock, not a fresh fix.
        && (0..MAX_FIX_AGE_MS).contains(&fix.age_ms)
}

/// P1-P6. `live_folder` answers "does this folder exist and is it not deleted".
pub fn route(places: &[PlaceRecord], fix: Option<Fix>, live_folder: &dyn Fn(FolderId) -> bool) -> Route {
    let live: Vec<&PlaceRecord> = places.iter().filter(|p| p.deleted.val.is_none()).collect();
    if live.is_empty() {
        return Route::Default; // P5
    }
    let fix = match fix {
        Some(f) if fix_usable(&f) => f,
        _ => return Route::Unsorted, // P4
    };
    live.into_iter()
        .filter(|p| live_folder(p.target.val)) // P6
        .filter(|p| distance_m(fix.lat, fix.lon, p.area.val.lat, p.area.val.lon) <= p.area.val.radius_m)
        // P3: smallest radius, then lowest id.
        .min_by(|a, b| {
            a.area
                .val
                .radius_m
                .total_cmp(&b.area.val.radius_m)
                .then_with(|| a.id.0.as_bytes().cmp(b.id.0.as_bytes()))
        })
        .map(|p| Route::Place { place: p.id, folder: p.target.val })
        .unwrap_or(Route::Default)
}

/// Haversine distance in metres.
pub fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlam = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_support::*;
    use uuid::Uuid;

    fn place(n: u128, lat: f64, lon: f64, radius: f64, folder: FolderId) -> PlaceRecord {
        PlaceRecord {
            id: PlaceId(Uuid::from_u128(n)),
            name: Versioned { v: GENESIS, val: format!("p{n}") },
            area: Versioned { v: GENESIS, val: Area { lat, lon, radius_m: radius } },
            target: Versioned { v: GENESIS, val: folder },
            deleted: Versioned { v: GENESIS, val: None },
        }
    }

    fn fix(lat: f64, lon: f64) -> Option<Fix> {
        Some(Fix { lat, lon, accuracy_m: 10.0, age_ms: 1000 })
    }

    const WORK: FolderId = FolderId(Uuid::from_u128(0x700));
    const SITE: FolderId = FolderId(Uuid::from_u128(0x701));
    fn all_live(_: FolderId) -> bool {
        true
    }

    // L22: haversine 1 degree of latitude = 111 195 m +- 1.
    #[test]
    fn haversine_one_degree() {
        let d = distance_m(0.0, 0.0, 1.0, 0.0);
        assert!((d - 111_195.0).abs() < 1.0, "{d}");
        assert!(distance_m(-30.5, 151.6, -30.5, 151.6) < 1e-6);
        // Symmetric and sane across the date line.
        let a = distance_m(10.0, 179.9, 10.0, -179.9);
        assert!((a - distance_m(10.0, -179.9, 10.0, 179.9)).abs() < 1e-6);
        assert!(a < 25_000.0, "{a}");
    }

    #[test]
    fn smallest_radius_wins_and_tie_goes_to_lowest_id() {
        let big = place(1, -30.0, 151.0, 500.0, WORK);
        let small = place(2, -30.0, 151.0, 100.0, SITE);
        let r = route(&[big.clone(), small.clone()], fix(-30.0, 151.0), &all_live);
        assert_eq!(r, Route::Place { place: small.id, folder: SITE });
        // Equal radius: lowest PlaceId, whatever the input order.
        let a = place(5, -30.0, 151.0, 100.0, WORK);
        let b = place(3, -30.0, 151.0, 100.0, SITE);
        assert_eq!(route(&[a.clone(), b.clone()], fix(-30.0, 151.0), &all_live), Route::Place { place: b.id, folder: SITE });
        assert_eq!(route(&[b, a], fix(-30.0, 151.0), &all_live), Route::Place { place: PlaceId(Uuid::from_u128(3)), folder: SITE });
    }

    #[test]
    fn containment_is_inclusive_at_the_radius() {
        let pl = place(1, 0.0, 0.0, 111_195.08, WORK);
        let just_inside = fix(0.999, 0.0);
        assert!(matches!(route(std::slice::from_ref(&pl), just_inside, &all_live), Route::Place { .. }));
        assert_eq!(route(std::slice::from_ref(&pl), fix(1.01, 0.0), &all_live), Route::Default, "outside every place -> Default");
    }

    // L22: stale fix, 101 m accuracy, no places, outside all, deleted target skipped.
    #[test]
    fn unusable_fix_goes_to_unsorted_only_when_places_exist() {
        let pl = place(1, -30.0, 151.0, 150.0, WORK);
        let stale = Some(Fix { lat: -30.0, lon: 151.0, accuracy_m: 10.0, age_ms: MAX_FIX_AGE_MS });
        assert_eq!(route(std::slice::from_ref(&pl), stale, &all_live), Route::Unsorted, "exactly 2 minutes is not 'under'");
        let fresh = Some(Fix { lat: -30.0, lon: 151.0, accuracy_m: 10.0, age_ms: MAX_FIX_AGE_MS - 1 });
        assert!(matches!(route(std::slice::from_ref(&pl), fresh, &all_live), Route::Place { .. }));
        let sloppy = Some(Fix { lat: -30.0, lon: 151.0, accuracy_m: 101.0, age_ms: 0 });
        assert_eq!(route(std::slice::from_ref(&pl), sloppy, &all_live), Route::Unsorted);
        let exact = Some(Fix { lat: -30.0, lon: 151.0, accuracy_m: 100.0, age_ms: 0 });
        assert!(matches!(route(std::slice::from_ref(&pl), exact, &all_live), Route::Place { .. }), "100 m is accurate enough");
        assert_eq!(route(std::slice::from_ref(&pl), None, &all_live), Route::Unsorted, "no fix, places exist");
        let future = Some(Fix { lat: -30.0, lon: 151.0, accuracy_m: 10.0, age_ms: -5 });
        assert_eq!(route(std::slice::from_ref(&pl), future, &all_live), Route::Unsorted);
        let nan = Some(Fix { lat: f64::NAN, lon: 151.0, accuracy_m: 10.0, age_ms: 0 });
        assert_eq!(route(std::slice::from_ref(&pl), nan, &all_live), Route::Unsorted);
        // No places at all: unknown location is just the default folder.
        assert_eq!(route(&[], None, &all_live), Route::Default);
        assert_eq!(route(&[], fix(0.0, 0.0), &all_live), Route::Default);
    }

    #[test]
    fn deleted_places_and_deleted_target_folders_are_skipped() {
        let mut deleted = place(1, -30.0, 151.0, 50.0, WORK);
        deleted.deleted = Versioned { v: v(2, p()), val: Some(T0) };
        let ok = place(2, -30.0, 151.0, 400.0, SITE);
        // The deleted (smaller) place is ignored.
        assert_eq!(route(&[deleted.clone(), ok.clone()], fix(-30.0, 151.0), &all_live), Route::Place { place: ok.id, folder: SITE });
        // All places deleted = no places = Default, even with no fix.
        assert_eq!(route(&[deleted], None, &all_live), Route::Default);
        // The smaller place points at a deleted folder: skipped, the larger one wins (P6).
        let small = place(3, -30.0, 151.0, 50.0, WORK);
        let live = |f: FolderId| f != WORK;
        assert_eq!(route(&[small.clone(), ok.clone()], fix(-30.0, 151.0), &live), Route::Place { place: ok.id, folder: SITE });
        // Only a dead-target place contains the fix: Default, never the dead folder.
        assert_eq!(route(&[small], fix(-30.0, 151.0), &live), Route::Default);
    }
}
