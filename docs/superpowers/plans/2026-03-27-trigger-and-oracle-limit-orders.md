# Trigger and Oracle-Offset Limit Orders Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow the swift server to accept TriggerMarket, TriggerLimit, and oracle-offset Limit orders.

**Architecture:** Refactor `validate_signed_order_params` from a flat if/else chain into a router that dispatches to per-order-type validator functions. Each validator mirrors the Drift program's validation rules for that order type.

**Tech Stack:** Rust, drift-rs types (OrderType, OrderParams, OrderTriggerCondition, PostOnlyParam, ErrorCode)

**Spec:** `docs/superpowers/specs/2026-03-27-trigger-and-oracle-limit-orders-design.md`

---

## File Map

- Modify: `src/swift_server.rs:920-969` — refactor `validate_signed_order_params`, add 4 new validator functions
- Modify: `src/swift_server.rs:50` — add `OrderTriggerCondition`, `PostOnlyParam` to imports
- Modify: `src/swift_server.rs` (tests module, ~line 1787+) — add new test cases

---

### Task 1: Add imports for new types

**Files:**
- Modify: `src/swift_server.rs:50-51`

- [ ] **Step 1: Add OrderTriggerCondition and PostOnlyParam to the drift_rs import**

In `src/swift_server.rs`, change the import block at line 50-51 from:

```rust
        CommitmentConfig, MarketId, MarketStatus, MarketType, OrderParams, OrderType,
        PositionDirection, ProgramError, SdkError, SdkResult, SignedMsgTriggerOrderParams,
```

to:

```rust
        CommitmentConfig, MarketId, MarketStatus, MarketType, OrderParams, OrderTriggerCondition,
        OrderType, PositionDirection, PostOnlyParam, ProgramError, SdkError, SdkResult,
        SignedMsgTriggerOrderParams,
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check 2>&1 | tail -5`
Expected: no errors (warnings are fine)

- [ ] **Step 3: Commit**

```bash
git add src/swift_server.rs
git commit -m "chore: import OrderTriggerCondition and PostOnlyParam types"
```

---

### Task 2: Extract `validate_market_order_params`

**Files:**
- Modify: `src/swift_server.rs:920-969`

- [ ] **Step 1: Write the failing test for market order validation**

Add this test to the `tests` module (after the existing `test_validate_auction_params` test):

```rust
#[test]
fn test_validate_market_order_params_extracted() {
    let min_order_size = 1 * LAMPORTS_PER_SOL;

    // Market long with valid auction: start <= end
    let params = create_test_order_params(
        OrderType::Market,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        Some((50, 99, 100)),
    );
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Market short with valid auction: start >= end
    let params = create_test_order_params(
        OrderType::Market,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Short,
        Some((50, 100, 99)),
    );
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Market with no auction params — invalid
    let params = create_test_order_params(
        OrderType::Market,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderAuction)
    );

    // Market long with reversed auction — invalid
    let params = create_test_order_params(
        OrderType::Market,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        Some((50, 100, 99)),
    );
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderAuction)
    );
}
```

- [ ] **Step 2: Run test to verify it passes (these are regression tests for existing behavior)**

Run: `cargo test test_validate_market_order_params_extracted -- --nocapture 2>&1 | tail -10`
Expected: PASS (existing logic already handles these cases)

- [ ] **Step 3: Extract `validate_market_order_params` and refactor the router**

Replace the entire `validate_signed_order_params` function (lines 920-969) with:

```rust
/// Simple validation from program's `handle_signed_order_ix`
fn validate_signed_order_params(
    taker_order_params: &OrderParams,
    min_order_size: u64,
) -> Result<(), ErrorCode> {
    if !matches!(taker_order_params.market_type, MarketType::Perp) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }

    if taker_order_params.base_asset_amount < min_order_size {
        // can always close reduce_only
        if !taker_order_params.reduce_only {
            log::info!(target: "server", "{} < {min_order_size}", taker_order_params.base_asset_amount);
            return Err(ErrorCode::InvalidOrderSizeTooSmall);
        }
    }

    match taker_order_params.order_type {
        OrderType::Market | OrderType::Oracle => validate_market_order_params(taker_order_params),
        OrderType::Limit => validate_limit_order_params(taker_order_params),
        OrderType::TriggerMarket => validate_trigger_market_order_params(taker_order_params),
        OrderType::TriggerLimit => validate_trigger_limit_order_params(taker_order_params),
    }
}

/// Validates Market and Oracle order auction params.
/// All three auction params must be present with correct price direction.
fn validate_market_order_params(params: &OrderParams) -> Result<(), ErrorCode> {
    if params.auction_duration.is_some()
        && params.auction_start_price.is_some()
        && params.auction_end_price.is_some()
    {
        let start_price = params.auction_start_price.unwrap();
        let end_price = params.auction_end_price.unwrap();

        if params.direction == PositionDirection::Long && start_price <= end_price
            || params.direction == PositionDirection::Short && start_price >= end_price
        {
            Ok(())
        } else {
            log::info!(target: "server", "auction price reversed");
            Err(ErrorCode::InvalidOrderAuction)
        }
    } else {
        Err(ErrorCode::InvalidOrderAuction)
    }
}

/// Validates Limit order params. Handles both regular limits (no auction params)
/// and oracle-offset limits (oracle_price_offset != 0 with oracle auction params).
fn validate_limit_order_params(params: &OrderParams) -> Result<(), ErrorCode> {
    if params.auction_duration.is_none()
        && params.auction_start_price.is_none()
        && params.auction_end_price.is_none()
    {
        Ok(())
    } else {
        Err(ErrorCode::InvalidOrderAuction)
    }
}

/// Validates TriggerMarket order params.
fn validate_trigger_market_order_params(_params: &OrderParams) -> Result<(), ErrorCode> {
    // placeholder — implemented in Task 4
    Err(ErrorCode::InvalidOrderMarketType)
}

/// Validates TriggerLimit order params.
fn validate_trigger_limit_order_params(_params: &OrderParams) -> Result<(), ErrorCode> {
    // placeholder — implemented in Task 5
    Err(ErrorCode::InvalidOrderMarketType)
}
```

Note: `validate_limit_order_params` keeps only the existing regular-limit logic for now. Oracle-offset support is added in Task 3. Trigger validators are placeholders until Tasks 4-5.

- [ ] **Step 4: Run all existing tests to verify no regressions**

Run: `cargo test test_validate -- --nocapture 2>&1 | tail -15`
Expected: all `test_validate_*` tests PASS

- [ ] **Step 5: Commit**

```bash
git add src/swift_server.rs
git commit -m "refactor: extract validate_market_order_params, router dispatch by order type"
```

---

### Task 3: Implement `validate_limit_order_params` with oracle-offset support

**Files:**
- Modify: `src/swift_server.rs` — replace `validate_limit_order_params` function
- Modify: `src/swift_server.rs` (tests module) — add oracle-offset limit tests

- [ ] **Step 1: Write failing tests for oracle-offset limit orders**

Add to the tests module:

```rust
#[test]
fn test_validate_oracle_offset_limit_order() {
    let min_order_size = 1 * LAMPORTS_PER_SOL;

    // Valid: oracle-offset limit long with auction, start <= end <= offset
    let mut params = create_test_order_params(
        OrderType::Limit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        Some((50, 100, 200)),
    );
    params.oracle_price_offset = Some(300);
    params.price = 0;
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Valid: oracle-offset limit short with auction, start >= end >= offset
    let mut params = create_test_order_params(
        OrderType::Limit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Short,
        Some((50, 200, 100)),
    );
    params.oracle_price_offset = Some(-300);
    params.price = 0;
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Valid: oracle-offset limit with no auction (start/end zero or None)
    let mut params = create_test_order_params(
        OrderType::Limit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.oracle_price_offset = Some(100);
    params.price = 0;
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Invalid: oracle-offset limit long, end > offset
    let mut params = create_test_order_params(
        OrderType::Limit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        Some((50, 100, 400)),
    );
    params.oracle_price_offset = Some(300);
    params.price = 0;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderAuction)
    );

    // Invalid: oracle-offset limit long, reversed auction (start > end)
    let mut params = create_test_order_params(
        OrderType::Limit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        Some((50, 200, 100)),
    );
    params.oracle_price_offset = Some(300);
    params.price = 0;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderAuction)
    );

    // Invalid: oracle-offset limit short, end < offset
    let mut params = create_test_order_params(
        OrderType::Limit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Short,
        Some((50, 200, 100)),
    );
    params.oracle_price_offset = Some(150);
    params.price = 0;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderAuction)
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_validate_oracle_offset_limit_order -- --nocapture 2>&1 | tail -10`
Expected: FAIL — oracle-offset orders currently rejected

- [ ] **Step 3: Implement `validate_limit_order_params` with oracle-offset support**

Replace the `validate_limit_order_params` function with:

```rust
/// Validates Limit order params. Handles both regular limits (no auction params)
/// and oracle-offset limits (oracle_price_offset != 0 with oracle auction params).
fn validate_limit_order_params(params: &OrderParams) -> Result<(), ErrorCode> {
    let is_oracle_offset = params.oracle_price_offset.is_some_and(|o| o != 0);

    if is_oracle_offset {
        let offset = params.oracle_price_offset.unwrap() as i64;
        let has_auction = params.auction_duration.is_some_and(|d| d > 0);

        if has_auction {
            let start = params
                .auction_start_price
                .ok_or(ErrorCode::InvalidOrderAuction)?;
            let end = params
                .auction_end_price
                .ok_or(ErrorCode::InvalidOrderAuction)?;

            if params.direction == PositionDirection::Long {
                if start <= end && end <= offset {
                    Ok(())
                } else {
                    Err(ErrorCode::InvalidOrderAuction)
                }
            } else if start >= end && end >= offset {
                Ok(())
            } else {
                Err(ErrorCode::InvalidOrderAuction)
            }
        } else {
            // no auction — start/end must be zero or None
            let start_ok = params.auction_start_price.map_or(true, |p| p == 0);
            let end_ok = params.auction_end_price.map_or(true, |p| p == 0);
            if start_ok && end_ok {
                Ok(())
            } else {
                Err(ErrorCode::InvalidOrderAuction)
            }
        }
    } else {
        // regular limit — no auction params allowed
        if params.auction_duration.is_none()
            && params.auction_start_price.is_none()
            && params.auction_end_price.is_none()
        {
            Ok(())
        } else {
            Err(ErrorCode::InvalidOrderAuction)
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_validate_oracle_offset_limit_order -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Run all validation tests for regressions**

Run: `cargo test test_validate -- --nocapture 2>&1 | tail -15`
Expected: all PASS

- [ ] **Step 6: Commit**

```bash
git add src/swift_server.rs
git commit -m "feat: support oracle-offset limit orders in validation"
```

---

### Task 4: Implement `validate_trigger_market_order_params`

**Files:**
- Modify: `src/swift_server.rs` — replace placeholder `validate_trigger_market_order_params`
- Modify: `src/swift_server.rs` (tests module) — add trigger market tests

- [ ] **Step 1: Write failing tests for trigger market orders**

Add to the tests module. Note: `create_test_order_params` sets `price: 1_000` by default, so trigger market tests need to override `price` to 0. Also need to set `trigger_price` and `trigger_condition` which aren't in the helper — set them after construction.

```rust
#[test]
fn test_validate_trigger_market_order() {
    let min_order_size = 1 * LAMPORTS_PER_SOL;

    // Valid trigger market order
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 0;
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Valid: trigger condition Below
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Short,
        None,
    );
    params.price = 0;
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Below;
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Invalid: trigger_price is None
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 0;
    params.trigger_price = None;
    params.trigger_condition = OrderTriggerCondition::Above;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidTriggerOrderCondition)
    );

    // Invalid: trigger_price is 0
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 0;
    params.trigger_price = Some(0);
    params.trigger_condition = OrderTriggerCondition::Above;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidTriggerOrderCondition)
    );

    // Invalid: price != 0 (trigger market must have no limit price)
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 50_000;
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderMarketType)
    );

    // Invalid: oracle_price_offset set
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 0;
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    params.oracle_price_offset = Some(100);
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderMarketType)
    );

    // Invalid: post_only set
    let mut params = create_test_order_params(
        OrderType::TriggerMarket,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 0;
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    params.post_only = PostOnlyParam::MustPostOnly;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderMarketType)
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_validate_trigger_market_order -- --nocapture 2>&1 | tail -10`
Expected: FAIL — placeholder returns `Err(InvalidOrderMarketType)` for all cases

- [ ] **Step 3: Implement `validate_trigger_market_order_params`**

Replace the placeholder function with:

```rust
/// Validates TriggerMarket order params.
/// Requires trigger_price, valid trigger_condition, price == 0, no oracle offset, no post_only.
fn validate_trigger_market_order_params(params: &OrderParams) -> Result<(), ErrorCode> {
    if !params.trigger_price.is_some_and(|p| p != 0) {
        return Err(ErrorCode::InvalidTriggerOrderCondition);
    }
    if !matches!(
        params.trigger_condition,
        OrderTriggerCondition::Above | OrderTriggerCondition::Below
    ) {
        return Err(ErrorCode::InvalidTriggerOrderCondition);
    }
    if params.price != 0 {
        return Err(ErrorCode::InvalidOrderMarketType);
    }
    if params.oracle_price_offset.is_some_and(|o| o != 0) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }
    if params.post_only != PostOnlyParam::None {
        return Err(ErrorCode::InvalidOrderMarketType);
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_validate_trigger_market_order -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/swift_server.rs
git commit -m "feat: implement validate_trigger_market_order_params"
```

---

### Task 5: Implement `validate_trigger_limit_order_params`

**Files:**
- Modify: `src/swift_server.rs` — replace placeholder `validate_trigger_limit_order_params`
- Modify: `src/swift_server.rs` (tests module) — add trigger limit tests

- [ ] **Step 1: Write failing tests for trigger limit orders**

Add to the tests module:

```rust
#[test]
fn test_validate_trigger_limit_order() {
    let min_order_size = 1 * LAMPORTS_PER_SOL;

    // Valid trigger limit order (price != 0)
    let mut params = create_test_order_params(
        OrderType::TriggerLimit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    // price is 1_000 from helper — non-zero, valid for trigger limit
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Valid: short with Below condition
    let mut params = create_test_order_params(
        OrderType::TriggerLimit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Short,
        None,
    );
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Below;
    assert!(validate_signed_order_params(&params, min_order_size).is_ok());

    // Invalid: price == 0 (trigger limit must have limit price)
    let mut params = create_test_order_params(
        OrderType::TriggerLimit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.price = 0;
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderMarketType)
    );

    // Invalid: trigger_price is None
    let mut params = create_test_order_params(
        OrderType::TriggerLimit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.trigger_price = None;
    params.trigger_condition = OrderTriggerCondition::Above;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidTriggerOrderCondition)
    );

    // Invalid: oracle_price_offset set
    let mut params = create_test_order_params(
        OrderType::TriggerLimit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    params.oracle_price_offset = Some(100);
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderMarketType)
    );

    // Invalid: post_only set
    let mut params = create_test_order_params(
        OrderType::TriggerLimit,
        MarketType::Perp,
        min_order_size,
        PositionDirection::Long,
        None,
    );
    params.trigger_price = Some(50_000);
    params.trigger_condition = OrderTriggerCondition::Above;
    params.post_only = PostOnlyParam::MustPostOnly;
    assert_eq!(
        validate_signed_order_params(&params, min_order_size),
        Err(ErrorCode::InvalidOrderMarketType)
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_validate_trigger_limit_order -- --nocapture 2>&1 | tail -10`
Expected: FAIL — placeholder returns `Err(InvalidOrderMarketType)` for all cases including valid ones

- [ ] **Step 3: Implement `validate_trigger_limit_order_params`**

Replace the placeholder function with:

```rust
/// Validates TriggerLimit order params.
/// Same as TriggerMarket except price must be non-zero (limit price required).
fn validate_trigger_limit_order_params(params: &OrderParams) -> Result<(), ErrorCode> {
    if !params.trigger_price.is_some_and(|p| p != 0) {
        return Err(ErrorCode::InvalidTriggerOrderCondition);
    }
    if !matches!(
        params.trigger_condition,
        OrderTriggerCondition::Above | OrderTriggerCondition::Below
    ) {
        return Err(ErrorCode::InvalidTriggerOrderCondition);
    }
    if params.price == 0 {
        return Err(ErrorCode::InvalidOrderMarketType);
    }
    if params.oracle_price_offset.is_some_and(|o| o != 0) {
        return Err(ErrorCode::InvalidOrderMarketType);
    }
    if params.post_only != PostOnlyParam::None {
        return Err(ErrorCode::InvalidOrderMarketType);
    }
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_validate_trigger_limit_order -- --nocapture 2>&1 | tail -10`
Expected: PASS

- [ ] **Step 5: Run full test suite**

Run: `cargo test 2>&1 | tail -15`
Expected: all tests PASS

- [ ] **Step 6: Commit**

```bash
git add src/swift_server.rs
git commit -m "feat: implement validate_trigger_limit_order_params"
```
