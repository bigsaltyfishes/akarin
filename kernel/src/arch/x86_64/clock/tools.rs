pub fn average_without_some_outliers(ticks: &mut [u64]) -> u64 {
    if ticks.is_empty() {
        return 0;
    }

    ticks.sort_unstable();
    let q1 = ticks[ticks.len() / 4];
    let q3 = ticks[ticks.len() * 3 / 4];

    let mut sum = 0u64;
    let mut count = 0u64;
    for tick in ticks {
        if *tick < q1 || *tick > q3 {
            continue;
        }
        sum = sum.saturating_add(*tick);
        count += 1;
    }
    if count == 0 {
        return 0;
    }
    sum.saturating_add(count - 1)
        .checked_div(count)
        .unwrap_or(0)
        .saturating_mul(1000)
}
