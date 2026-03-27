# Design: Support Trigger Orders and Oracle-Offset Limit Orders

## Goal

Allow the swift server to accept TriggerMarket, TriggerLimit, and oracle-offset Limit orders, in addition to the existing Market, Oracle, and regular Limit orders.

## Background

The swift server's `validate_signed_order_params` currently restricts order types to `Market | Oracle | Limit` and validates auction params in a flat if/else chain. This blocks trigger orders and oracle-offset limit orders from being submitted.

The Drift program's `OrderType` enum has 5 variants: Market, Limit, TriggerMarket, TriggerLimit, Oracle. Trigger orders have a two-phase lifecycle — validated without auction params at placement, then assigned auction params dynamically when the oracle hits the trigger price. Oracle-offset limit orders are `OrderType::Limit` with `oracle_price_offset != 0`.

## Design

### Refactor `validate_signed_order_params` into router + per-type validators

**Router** (`validate_signed_order_params`):
- Keeps shared checks: market type must be Perp, base_asset_amount >= min_order_size (unless reduce_only)
- Dispatches to per-type validator based on `order_type`:
  - `Market | Oracle` -> `validate_market_order_params`
  - `Limit` -> `validate_limit_order_params`
  - `TriggerMarket` -> `validate_trigger_market_order_params`
  - `TriggerLimit` -> `validate_trigger_limit_order_params`

**`validate_market_order_params`** (Market and Oracle):
- All three auction params must be `Some`
- Long: auction_start_price <= auction_end_price
- Short: auction_start_price >= auction_end_price

**`validate_limit_order_params`** — two sub-paths:
1. Oracle-offset limit (`oracle_price_offset` is `Some` and non-zero):
   - If has auction (`auction_duration` > 0): auction_start_price and auction_end_price required. Long: start <= end <= offset. Short: start >= end >= offset.
   - If no auction: auction_start_price and auction_end_price must be zero or None.
2. Regular limit (`oracle_price_offset` is None or zero):
   - All three auction params must be None (unchanged from current behavior).

**`validate_trigger_market_order_params`**:
- `trigger_price` must be Some and non-zero
- `trigger_condition` must be Above or Below
- `price` must be 0 (no limit price)
- `oracle_price_offset` must be None or 0
- `post_only` must be None

**`validate_trigger_limit_order_params`**:
- Same as trigger market except `price` must be non-zero

### Test coverage

New test cases:
1. Trigger market: valid order, invalid (trigger_price == 0, price != 0, oracle_price_offset set, post_only set)
2. Trigger limit: valid order, invalid (price == 0, same invalid cases)
3. Oracle-offset limit: valid with auction, valid without auction, invalid (reversed direction, end past offset)
4. Existing tests unchanged

## Scope

Single file change: `src/swift_server.rs`. No changes to ws_server, message types, or dependencies. All required types (TriggerMarket, TriggerLimit, OrderTriggerCondition, oracle_price_offset) already exist in drift-rs.
