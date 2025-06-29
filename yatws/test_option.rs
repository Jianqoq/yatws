// yatws/test_option.rs
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use log::{error, info, warn};
use std::time::Duration;
use chrono::{Utc, Duration as ChronoDuration, NaiveDate, Datelike};
use yatws::{
  IBKRError,
  IBKRClient,
  OptionsStrategyBuilder,
  contract::{Contract, SecType, OptionRight, DateOrMonth},
  data::{MarketDataType, TickOptionComputationData},
  data_ref_manager::DataRefManager,
};

pub(super) fn box_spread_yield_impl(client: &IBKRClient, _is_live: bool) -> Result<()> {
  info!("--- Testing Box Spread Yield Calculation ---");
  let data_market = client.data_market();
  let data_ref = client.data_ref(); // Need this for the builder

  // Define underlyings and parameters - Use Futures now
  let underlyings = [
    ("ES", SecType::Future, "CME"), // S&P E-mini
    // Broken. Perhaps I need to set the M5 (monthly 2025) local symbol.
    //("RTY", SecType::Future, "CME"), // Russell 2000 E-mini
  ];
  let strike_ratios = [0.05, 0.01]; // Strike ratios to underlying
  let expiry_offsets_days = [30, 60, 90]; // Approx days from today

  let today = Utc::now().date_naive();
  let target_expiries: Vec<NaiveDate> = expiry_offsets_days
    .iter()
    .map(|&days| today + ChronoDuration::days(days))
    .collect();

  let mut overall_success = true;

  for (symbol, sec_type, exchange) in underlyings {
    info!("--- Testing Underlying: {} ({}) ---", symbol, sec_type);

    let mut uc = Contract::new();
    uc.symbol = symbol.to_string();
    uc.sec_type = SecType::Future;
    uc.exchange = exchange.to_string();
    // Take the nearest future. In theory we could take the one matching the option expiration date.
    let futs: Vec<_> = data_ref.get_contract_details(&uc)?.into_iter().map(|d| d.contract_month).collect();
    log::info!("Futures: {:?}", futs);
    assert!(!futs.is_empty(), "No contracts found for future symbol: {}", symbol);
    uc.last_trade_date_or_contract_month = futs.into_iter().min().unwrap().map(|x| DateOrMonth::Month(x));
    // No last trade, use ask for now, assuming that the spreads are tight.
    let underlying_price = data_market.get_quote(&uc, Some(MarketDataType::Delayed), Duration::from_secs(10))?.1.unwrap();
    if underlying_price <= 0.0 {
      warn!("Invalid price ({}) for {}, strike selection might be inaccurate.", underlying_price, symbol);
    }
    log::info!("Underlying price: {} = {:.2}", symbol, underlying_price);

    for target_expiry in &target_expiries {
      for &strike_ratio in &strike_ratios {
        let target_strike1 = underlying_price * (1. - 0.5 * strike_ratio);
        let target_strike2 = underlying_price * (1. + 0.5 * strike_ratio);

        info!("Attempting Box for {} Exp~{}, Strikes~{:.2}/{:.2}",
              symbol, target_expiry.format("%Y-%m-%d"), target_strike1, target_strike2);

        // Use OptionsStrategyBuilder to define the box
        let builder_result = OptionsStrategyBuilder::new(
          data_ref.clone(), // Clone Arc
          symbol,
          underlying_price,
          1.0, // Quantity = 1 box
          sec_type.clone(),
        )?
          .box_spread_nearest_expiry(*target_expiry, target_strike1, target_strike2);

        let builder = match builder_result {
          Ok(b) => b,
          Err(e) => {
            error!("Failed to define box strategy for {} Exp~{}: {:?}", symbol, target_expiry, e);
            overall_success = false;
            continue; // Try next parameters
          }
        };

        // Build the combo contract
        let (combo_contract, _order_request) = match builder.build() {
          Ok(result) => result,
          Err(e) => {
            error!("Failed to build combo contract for {} Exp~{}: {:?}", symbol, target_expiry, e);
            overall_success = false;
            continue;
          }
        };

        // Extract actual strikes and expiry from the built contract for yield calculation
        // This requires parsing the combo legs or relying on the builder's internal state (which isn't exposed)
        // Let's re-extract from combo legs for robustness
        let mut strikes = Vec::new();
        let mut expiry = None;
        for leg in &combo_contract.combo_legs {
          // Fetch full contract details for the leg to get strike/expiry
          // This is inefficient but necessary if builder doesn't expose details
          let leg_contract_spec = Contract { con_id: leg.con_id, ..Default::default() };
          match data_ref.get_contract_details(&leg_contract_spec) {
            Ok(details_list) if !details_list.is_empty() => {
              let leg_details = &details_list[0].contract;
              if let Some(s) = leg_details.strike { strikes.push(s); }
              if expiry.is_none() { expiry = leg_details.last_trade_date_or_contract_month.clone(); }
            },
            Ok(_) => { error!("Leg contract details not found for conId {}", leg.con_id); overall_success = false; break; },
            Err(e) => { error!("Error fetching leg details for conId {}: {:?}", leg.con_id, e); overall_success = false; break; },
          }
        }
        if !overall_success { continue; } // Skip if leg details failed

        strikes.sort_by(|a, b| a.partial_cmp(b).unwrap());
        strikes.dedup();
        if strikes.len() != 2 {
          error!("Could not determine unique strike pair from combo legs: {:?}", strikes);
          overall_success = false;
          continue;
        }
        let actual_strike1 = strikes[0];
        let actual_strike2 = strikes[1];
        let actual_strike_diff = actual_strike2 - actual_strike1;

        let actual_expiry_date = if let Some(expiry) = expiry {
          if let DateOrMonth::Date(date) = expiry {
            date
          } else {
            error!("Could not determine expiry date from combo legs expiry: {}.", expiry);
            overall_success = false;
            continue;
          }
        } else {
          error!("Could not determine expiry date from combo legs expiry (None).");
          overall_success = false;
          continue;
        };

        info!("  Actual Box: Exp={}, Strikes={:.2}/{:.2} (Diff={:.2})",
              actual_expiry_date.format("%Y%m%d"), actual_strike1, actual_strike2, actual_strike_diff);

        // Get quote for the combo contract
        let quote_timeout = Duration::from_secs(20);
        match data_market.get_quote(&combo_contract, Some(MarketDataType::Delayed), quote_timeout) {
          Ok((Some(bid), Some(ask), _last)) => {
            let mid_price = (bid + ask) / 2.0;
            info!("  Quote: Bid={:.4}, Ask={:.4}, Mid={:.4}", bid, ask, mid_price);

            // Calculate yield
            let days_to_expiry = (actual_expiry_date - today).num_days();
            if days_to_expiry <= 0 {
              warn!("  Expiry date {} is not in the future. Cannot calculate yield.", actual_expiry_date);
              continue;
            }
            let time_to_expiry_years = days_to_expiry as f64 / 365.0;

            if mid_price <= 0.0 || mid_price >= actual_strike_diff {
              warn!("  Mid price ({:.4}) is invalid relative to strike difference ({:.2}). Cannot calculate yield.", mid_price, actual_strike_diff);
              continue;
            }

            let ratio = mid_price / actual_strike_diff;
            let yield_pct = -ratio.ln() / time_to_expiry_years * 100.0;
            info!("  => {}:{}/{:.2} Calculated Annual Yield: {:.4}%", symbol, actual_expiry_date.format("%Y%m"), ratio, yield_pct);

          }
          Ok((bid, ask, _)) => {
            error!("  Failed to get valid Bid/Ask quote for combo. Bid: {:?}, Ask: {:?}", bid, ask);
            overall_success = false;
          }
          Err(e) => {
            error!("  Error getting quote for combo {:?}: {:?}", combo_contract, e);
            overall_success = false;
          }
        }
        // Add a small delay to avoid pacing violations, especially in live mode
        std::thread::sleep(Duration::from_secs(2));
      }
    }
  }

  if overall_success {
    info!("Box spread yield test completed successfully (individual quote checks passed/failed as logged).");
    Ok(())
  } else {
    Err(anyhow!("One or more errors occurred during box spread yield test."))
  }
}

pub(super) fn option_calculations_impl(client: &IBKRClient, _is_live: bool) -> Result<()> {
  info!("--- Testing Option Calculations (Implied Vol & Option Price) ---");
  let data_mgr = client.data_market();
  let ref_data_mgr = client.data_ref();
  let timeout = Duration::from_secs(20);

  // 1. Get AAPL stock price
  let aapl_stock_contract = Contract::stock("AAPL");
  info!("Fetching current price for AAPL...");
  let (_bid, _ask, last_price_opt) = data_mgr.get_quote(&aapl_stock_contract, Some(MarketDataType::Delayed), timeout)
    .context("Failed to get quote for AAPL stock")?;
  let under_price = match last_price_opt {
    Some(price) if price > 0.0 => price,
    _ => {
      warn!("Could not get valid last price for AAPL. Using placeholder 170.0 for underlying price.");
      170.0 // Placeholder if live price fails
    }
  };
  info!("Using underlying AAPL price: {:.2}", under_price);

  // 2. Define an AAPL call option contract
  //    - Find next month's 3rd Friday for expiry
  //    - Strike price ~10% above current stock price
  let today = Utc::now();
  let mut current_month = today.month();
  let mut current_year = today.year();
  if current_month == 12 {
    current_month = 1;
    current_year += 1;
  } else {
    current_month += 1;
  }
  let first_of_next_month = NaiveDate::from_ymd_opt(current_year, current_month, 1).unwrap();
  let days_to_friday = (chrono::Weekday::Fri.number_from_monday() + 7 - first_of_next_month.weekday().number_from_monday()) % 7;
  let first_friday = first_of_next_month + ChronoDuration::days(days_to_friday as i64);
  let target_expiry_date = first_friday + ChronoDuration::weeks(2); // 3rd Friday

  let target_strike_raw = under_price * 1.10;
  // Round to nearest $2.50 increment for typical AAPL options, or $5 for higher prices
  let strike_increment = if target_strike_raw < 200.0 { 2.5 } else { 5.0 };
  let strike_price = (target_strike_raw / strike_increment).round() * strike_increment;

  info!("Targeting AAPL Call Option: Expiry={}, Strike={:.2}", target_expiry_date, strike_price);

  let option_contract_spec = Contract::option("AAPL", &target_expiry_date, strike_price, OptionRight::Call, "SMART", "USD");

  // Get full contract details to ensure it's valid and get con_id
  info!("Fetching contract details for the target option...");
  let option_details_list = ref_data_mgr.get_contract_details(&option_contract_spec)
    .context(format!("Failed to get contract details for AAPL option {} C{}", target_expiry_date, strike_price))?;

  if option_details_list.is_empty() {
    return Err(anyhow!("No contract details found for the specified AAPL option. Check expiry/strike or market data subscription."));
  }
  let option_contract = option_details_list[0].contract.clone();
  info!("Using option contract: ConID={}, LocalSymbol={}", option_contract.con_id, option_contract.local_symbol.as_deref().unwrap_or("N/A"));


  // 3. Calculate Implied Volatility
  let placeholder_option_price = 2.50; // Placeholder market price for the option
  info!("Calculating Implied Volatility for {} with OptionPrice={}, UnderPrice={}...",
        option_contract.local_symbol.as_deref().unwrap_or("AAPL Option"), placeholder_option_price, under_price);

  match data_mgr.calculate_implied_volatility(&option_contract, placeholder_option_price, under_price, timeout) {
    Ok(computation) => {
      info!("Successfully calculated Implied Volatility:");
      log_tick_option_computation(&computation);
    }
    Err(e) => {
      error!("Failed to calculate Implied Volatility: {:?}", e);
      // Don't fail the whole test, proceed to option price calc
    }
  }

  std::thread::sleep(Duration::from_secs(1));

  // 4. Calculate Option Price
  let placeholder_volatility = 0.30; // Placeholder volatility (30%)
  info!("Calculating Option Price for {} with Volatility={}, UnderPrice={}...",
        option_contract.local_symbol.as_deref().unwrap_or("AAPL Option"), placeholder_volatility, under_price);

  match data_mgr.calculate_option_price(&option_contract, placeholder_volatility, 170.0 /* under_price */, timeout) {
    Ok(computation) => {
      info!("Successfully calculated Option Price:");
      log_tick_option_computation(&computation);
    }
    Err(e) => {
      error!("Failed to calculate Option Price: {:?}", e);
      // Don't fail the whole test if this part fails
    }
  }

  Ok(())
}

fn log_tick_option_computation(computation: &TickOptionComputationData) {
  info!("  TickType: {:?}", computation.tick_type);
  info!("  TickAttrib: {:?}", computation.tick_attrib);
  info!("  ImpliedVol: {:?}", computation.implied_vol);
  info!("  Delta: {:?}", computation.delta);
  info!("  OptPrice: {:?}", computation.opt_price);
  info!("  PvDividend: {:?}", computation.pv_dividend);
  info!("  Gamma: {:?}", computation.gamma);
  info!("  Vega: {:?}", computation.vega);
  info!("  Theta: {:?}", computation.theta);
  info!("  UndPrice: {:?}", computation.und_price);
}

pub(super) fn options_strategy_builder_test_impl(client: &IBKRClient, _is_live: bool) -> Result<()> {
  info!("--- Testing OptionsStrategyBuilder - All Strategy Types ---");
  let data_market = client.data_market();
  let data_ref = client.data_ref();

  // Define underlyings to test with - focusing on liquid instruments
  let underlyings = [
    ("AAPL", SecType::Stock, "SMART", "USD"), // S&P 500 ETF
  ];

  // Track which strategies failed
  let mut failed_strategies: Vec<String> = Vec::new();
  let mut total_strategies = 0;

  for (symbol, sec_type, exchange, currency) in underlyings {
    info!("--- Testing strategies for: {} ({}) ---", symbol, sec_type);

    // Get underlying price
    let mut contract = Contract::new();
    contract.symbol = symbol.to_string();
    contract.sec_type = sec_type.clone();
    contract.exchange = exchange.to_string();
    contract.currency = currency.to_string();

    let quote_result = data_market.get_quote(&contract, Some(MarketDataType::Delayed), Duration::from_secs(10));
    let underlying_price = match quote_result {
      Ok((_, Some(ask), _)) => ask,
      Ok((Some(bid), _, _)) => bid,
      Ok((_, _, Some(last))) => last,
      _ => {
        warn!("Couldn't get price for {}. Using placeholder 100.", symbol);
        100.0 // Placeholder price
      }
    };

    info!("Using underlying price: {:.2} for {}", underlying_price, symbol);

    // Get real option chain data to use actual strikes and expiries
    let underlying_details = match data_ref.get_contract_details(&contract) {
      Ok(details) if !details.is_empty() => details[0].contract.con_id,
      _ => {
        error!("Failed to get underlying contract details for {}", symbol);
        failed_strategies.push("INIT".to_string());
        continue;
      }
    };

    let option_params = match data_ref.get_option_chain_params(
      symbol, "", sec_type.clone(), underlying_details
    ) {
      Ok(params) if !params.is_empty() => params,
      Ok(_) => {
        warn!("Empty option chain parameters for {}", symbol);
        continue;
      },
      Err(e) => {
        error!("Failed to get option chain for {}: {}", symbol, e);
        failed_strategies.push("INIT".to_string());
        continue;
      }
    };

    let primary_params = &option_params[0];
    if primary_params.strikes.len() < 4 || primary_params.expirations.len() < 2 {
      warn!("Insufficient option data for {} (need at least 4 strikes and 2 expiries)", symbol);
      continue;
    }

    // Use real strikes from option chain, find ones relative to underlying price
    // Use real strikes from option chain, find ones relative to underlying price
    let available_strikes = &primary_params.strikes;

    // Sort strikes and ensure we have enough for complex strategies
    let mut sorted_strikes = available_strikes.clone();
    sorted_strikes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    sorted_strikes.dedup(); // Remove duplicates

    if sorted_strikes.len() < 4 {
      warn!("Insufficient distinct strikes for {} (need at least 4, have {})",
            symbol, sorted_strikes.len());
      continue;
    }

    let atm_strike = sorted_strikes
      .iter()
      .min_by(|a, b| (*a - underlying_price).abs().partial_cmp(&(*b - underlying_price).abs()).unwrap())
      .cloned()
      .unwrap();

    // Select 4 distinct strikes from the sorted list, ensuring proper ordering
    // Strategy: Pick evenly spaced strikes from the available range
    let num_strikes = sorted_strikes.len();

    let (strike1, strike2, strike3, strike4) = if num_strikes >= 7 {
      // If we have plenty of strikes, pick every other one for good spacing
      let start_idx = std::cmp::max(0, (num_strikes / 2).saturating_sub(3));
      (
        sorted_strikes[start_idx],
        sorted_strikes[start_idx + 2],
        sorted_strikes[start_idx + 4],
        sorted_strikes[start_idx + 6]
      )
    } else if num_strikes >= 4 {
      // Use consecutive strikes but ensure they span a reasonable range
      let quarter = num_strikes / 4;
      (
        sorted_strikes[0],
        sorted_strikes[quarter],
        sorted_strikes[quarter * 2],
        sorted_strikes[quarter * 3]
      )
    } else {
      // Fallback - shouldn't reach here due to earlier check
      (
        sorted_strikes[0],
        sorted_strikes[1],
        sorted_strikes[2],
        sorted_strikes[3]
      )
    };

    // Double-check that our strikes are properly ordered (they should be by construction)
    assert!(strike1 < strike2 && strike2 < strike3 && strike3 < strike4,
            "Strike selection failed: {:.2} < {:.2} < {:.2} < {:.2}",
            strike1, strike2, strike3, strike4);

    info!("Selected strikes: {:.2}, {:.2}, {:.2}, {:.2} (ATM: {:.2}, Available: {})",
          strike1, strike2, strike3, strike4, atm_strike, num_strikes);

    // Use real expiration dates from option chain
    let available_expiries = &primary_params.expirations;
    let expiry1_str = &available_expiries[0];
    let expiry2_str = available_expiries.get(1).unwrap_or(&available_expiries[0]);

    let expiry1 = NaiveDate::parse_from_str(expiry1_str, "%Y%m%d")
      .context(format!("Failed to parse expiry: {}", expiry1_str))?;
    let expiry2 = NaiveDate::parse_from_str(expiry2_str, "%Y%m%d")
      .context(format!("Failed to parse expiry: {}", expiry2_str))?;

    info!("Real expiries: {} ({}), {} ({})", expiry1_str, expiry1.format("%Y-%m-%d"), expiry2_str, expiry2.format("%Y-%m-%d"));

    // Test each strategy type - we now guarantee distinct strikes
    // 1. Single leg options
    info!("Testing single leg options...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.buy_call(expiry1, strike3), "Buy Call", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.sell_call(expiry1, strike3), "Sell Call", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.buy_put(expiry1, strike2), "Buy Put", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.sell_put(expiry1, strike2), "Sell Put", &mut failed_strategies, &mut total_strategies);

    // 2. Vertical spreads
    info!("Testing vertical spreads...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.bull_call_spread(expiry1, strike2, strike3), "Bull Call Spread", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.bear_call_spread(expiry1, strike2, strike3), "Bear Call Spread", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.bull_put_spread(expiry1, strike2, strike3), "Bull Put Spread", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.bear_put_spread(expiry1, strike2, strike3), "Bear Put Spread", &mut failed_strategies, &mut total_strategies);

    // 3. Straddles/Strangles
    info!("Testing straddles and strangles...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_straddle(expiry1, atm_strike), "Long Straddle", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.short_straddle(expiry1, atm_strike), "Short Straddle", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_strangle(expiry1, strike3, strike2), "Long Strangle", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.short_strangle(expiry1, strike3, strike2), "Short Strangle", &mut failed_strategies, &mut total_strategies);

    // 4. Box spread
    info!("Testing box spread...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.box_spread_nearest_expiry(expiry1, strike2, strike3), "Box Spread", &mut failed_strategies, &mut total_strategies);

    // 5. Stock-related strategies (option legs only)
    info!("Testing stock-related strategies (option legs only)...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.collar_options(expiry1, strike2, strike3), "Collar Options", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.covered_call_option(expiry1, strike3), "Covered Call Option", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.covered_put_option(expiry1, strike2), "Covered Put Option", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.protective_put_option(expiry1, strike2), "Protective Put Option", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.stock_repair_options(expiry1, strike2, strike3), "Stock Repair Options", &mut failed_strategies, &mut total_strategies);

    // 6. Ratio spreads
    info!("Testing ratio spreads...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_ratio_call_spread(expiry1, strike2, strike3, 1, 2), "Long Ratio Call Spread", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_ratio_put_spread(expiry1, strike2, strike3, 2, 1), "Long Ratio Put Spread", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.short_ratio_put_spread(expiry1, strike2, strike3, 2, 1), "Short Ratio Put Spread", &mut failed_strategies, &mut total_strategies);

    // 7. Butterflies
    info!("Testing butterflies...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_put_butterfly(expiry1, strike1, strike2, strike3), "Long Put Butterfly", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.short_call_butterfly(expiry1, strike1, strike2, strike3), "Short Call Butterfly", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_iron_butterfly(expiry1, strike1, strike2, strike3), "Long Iron Butterfly", &mut failed_strategies, &mut total_strategies);

    // 8. Condors
    info!("Testing condors...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_put_condor(expiry1, strike1, strike2, strike3, strike4), "Long Put Condor", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.short_condor(expiry1, strike1, strike2, strike3, strike4), "Short Condor", &mut failed_strategies, &mut total_strategies);

    // 9. Calendar spreads
    info!("Testing calendar spreads...");
    if expiry1 != expiry2 {
      test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.long_put_calendar_spread(atm_strike, expiry1, expiry2), "Long Put Calendar Spread", &mut failed_strategies, &mut total_strategies);
      test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.short_call_calendar_spread(atm_strike, expiry1, expiry2), "Short Call Calendar Spread", &mut failed_strategies, &mut total_strategies);
    } else {
      warn!("Skipping calendar spreads - need different expiries, both are: {}", expiry1_str);
    }

    // 10. Synthetics
    info!("Testing synthetics...");
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.synthetic_long_put_option(expiry1, atm_strike), "Synthetic Long Put Option", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.synthetic_long_stock(expiry1, atm_strike), "Synthetic Long Stock", &mut failed_strategies, &mut total_strategies);
    test_single_strategy(create_builder(data_ref.clone(), symbol, underlying_price, sec_type.clone())?.synthetic_short_stock(expiry1, atm_strike), "Synthetic Short Stock", &mut failed_strategies, &mut total_strategies);

    // Add a small delay to avoid hammering the API
    std::thread::sleep(Duration::from_secs(1));
  }

  let successful_strategies = total_strategies - failed_strategies.len();

  info!("=== OptionsStrategyBuilder Test Results ===");
  info!("Total strategies tested: {}", total_strategies);
  info!("Successful strategies: {}", successful_strategies);
  info!("Failed strategies: {}", failed_strategies.len());

  if !failed_strategies.is_empty() {
    error!("Failed strategies:");
    for strategy in &failed_strategies {
      error!("  - {}", strategy);
    }
  }

  if failed_strategies.is_empty() {
    info!("OptionsStrategyBuilder test completed successfully - all strategies passed!");
    Ok(())
  } else {
    Err(anyhow!("OptionsStrategyBuilder test failed: {}/{} strategies failed: {:?}",
                failed_strategies.len(), total_strategies, failed_strategies))
  }
}

// Helper function to create a builder
fn create_builder(
  data_ref: Arc<DataRefManager>,
  symbol: &str,
  underlying_price: f64,
  sec_type: SecType
) -> Result<OptionsStrategyBuilder, IBKRError> {
  OptionsStrategyBuilder::new(
    data_ref,
    symbol,
    underlying_price,
    1.0, // Quantity = 1
    sec_type,
  )
}

// Helper function to test a strategy
fn test_single_strategy(
  result: Result<OptionsStrategyBuilder, IBKRError>,
  strategy_name: &str,
  failed_strategies: &mut Vec<String>,
  total_strategies: &mut usize,
) {
  *total_strategies += 1;

  match result {
    Ok(builder) => {
      info!("  Successfully created {} strategy", strategy_name);

      // Try to build the contract and order request
      match builder.build() {
        Ok((contract, _order_request)) => {
          info!("  ✓ Successfully built contract for {} with {} legs",
                strategy_name, contract.combo_legs.len());
        },
        Err(e) => {
          error!("  ✗ Failed to build {} strategy: {:?}", strategy_name, e);
          failed_strategies.push(format!("{} (build failed)", strategy_name));
        }
      }
    },
    Err(e) => {
      error!("  ✗ Unable to create {} strategy: {:?}", strategy_name, e);
      failed_strategies.push(format!("{} (creation failed)", strategy_name));
    }
  }
}
