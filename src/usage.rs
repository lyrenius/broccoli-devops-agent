//! Token accounting and cost: what the model relay was asked to do, and what it costs.
//!
//! The split of responsibilities matters. Token counts are facts a backend reports, so they are
//! recorded on the Job and in the EventLog and never recomputed. Money is not a fact: it is those
//! counts multiplied by a price list that lives in the operator's config and can change between
//! two readings of the same record. So no cost is ever stored — every figure here is derived on
//! demand from the counts plus the configured [`Pricing`], and a deployment with no price list
//! still gets complete token accounting with the cost simply absent.
//!
//! The same asymmetry decides how a gap is reported. A relay that omits its `usage` block leaves
//! tokens unknown for that request; those requests are counted separately (`requests_without_usage`)
//! and every derived figure says so, because a bill quietly rendered as zero is worse than one
//! marked incomplete.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

use crate::domain::{EventRecord, ModelUsage};

/// Tokens per price unit: prices are quoted per million tokens, as every relay quotes them.
const TOKENS_PER_PRICE_UNIT: f64 = 1_000_000.0;

/// Event kind under which one pass's usage is recorded in the EventLog.
pub const USAGE_EVENT_KIND: &str = "model.usage";

/// Durable start of an individual model request.
pub const REQUEST_STARTED: &str = "model.request_started";
/// Durable completion of an individual model request.
pub const REQUEST_FINISHED: &str = "model.request_finished";

/// Provider-independent ledger entry. Start and finish share the same request ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestUsage {
    /// Stable ID across retries of event delivery and log replay.
    pub request_id: String,
    /// Job ID or standalone diagnostic run ID used to group attempts into passes.
    pub namespace: Uuid,
    /// Configured provider model.
    pub model: String,
    /// Associated Issue, absent for diagnostics.
    pub issue_id: Option<crate::domain::IssueId>,
    /// Associated Job, absent for diagnostics.
    pub job_id: Option<crate::domain::JobId>,
    /// Model turn, distinct from request attempts when there are retries.
    pub turn: u32,
    /// Start time.
    pub started_at: DateTime<Utc>,
    /// End time, absent for in-flight or interrupted requests.
    pub finished_at: Option<DateTime<Utc>>,
    /// started, succeeded, failed, cancelled, interrupted or not_sent.
    pub status: String,
    /// Counts actually reported, absent while in flight or when lost during a restart.
    pub usage: Option<ModelUsage>,
    /// Backend or recovery explanation.
    pub error: Option<String>,
}

impl RequestUsage {
    /// Known counters. A started request without a finish is explicitly unknown, never free.
    pub fn counts(&self) -> ModelUsage {
        if self.status == "not_sent" {
            return ModelUsage {
                model: self.model.clone(),
                ..Default::default()
            };
        }
        self.usage.clone().unwrap_or_else(|| ModelUsage {
            model: self.model.clone(),
            requests: 1,
            requests_without_usage: 1,
            ..Default::default()
        })
    }
}

/// One attempt with a price computed from its known counters for operator display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestCost {
    /// Ledger entry; exported as flat fields in the API.
    #[serde(flatten)]
    pub request: RequestUsage,
    /// Cost of the known portion; absent when completely unknown or no pricing exists.
    pub cost: Option<f64>,
    /// Currency of the configured price table.
    pub currency: Option<String>,
}

/// Folds duplicate events and prefers the first terminal record over start/repeated records.
pub fn request_ledger(events: &[EventRecord]) -> Vec<RequestUsage> {
    let mut records: BTreeMap<String, RequestUsage> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|e| e.kind == REQUEST_STARTED || e.kind == REQUEST_FINISHED)
    {
        let Ok(record) = serde_json::from_value::<RequestUsage>(event.payload.clone()) else {
            continue;
        };
        if records
            .get(&record.request_id)
            .is_some_and(|old| old.status != "started")
        {
            continue;
        }
        records.insert(record.request_id.clone(), record);
    }
    let mut records: Vec<_> = records.into_values().collect();
    records.sort_by_key(|r| (r.started_at, r.request_id.clone()));
    records
}

/// What the configured relay charges, per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pricing {
    /// Price per million input tokens.
    pub input_per_mtok: f64,
    /// Price per million input tokens that hit the prompt cache. Omitted means cached tokens are
    /// billed at the full input rate, which is the safe assumption: it can only overstate.
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    /// Price per million output tokens.
    pub output_per_mtok: f64,
    /// Currency label for display only; no conversion is ever performed.
    #[serde(default = "default_currency")]
    pub currency: String,
}

fn default_currency() -> String {
    "USD".to_string()
}

impl Pricing {
    /// Cost of one usage record under this price list.
    ///
    /// Cached tokens are a subset of the input tokens, not an addition, so they are billed once —
    /// at the cached rate when one is configured, at the full input rate otherwise.
    pub fn cost(&self, usage: &ModelUsage) -> f64 {
        let cached = usage.cached_input_tokens.min(usage.input_tokens);
        let fresh = usage.input_tokens - cached;
        let cached_rate = self.cached_input_per_mtok.unwrap_or(self.input_per_mtok);
        (fresh as f64 * self.input_per_mtok
            + cached as f64 * cached_rate
            + usage.output_tokens as f64 * self.output_per_mtok)
            / TOKENS_PER_PRICE_UNIT
    }
}

/// A ceiling on what the whole deployment may spend before it stops working on its own.
///
/// Zero on both fields means no ceiling. The check is deliberately cumulative over the data
/// directory's whole event log rather than per run: a bill is the sum of every call, and a limit
/// that resets each pass would not be a limit at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SpendBudget {
    /// Total tokens (input plus output) across every recorded pass. Zero disables the check.
    pub max_total_tokens: u64,
    /// Total cost across every recorded pass, in the pricing currency. Zero disables the check;
    /// a cost ceiling needs `[model.pricing]` to be configured, or nothing can be priced.
    pub max_total_cost: f64,
}

impl SpendBudget {
    /// Whether any ceiling is configured at all.
    pub fn is_set(&self) -> bool {
        self.max_total_tokens > 0 || self.max_total_cost > 0.0
    }

    /// The reason the budget is spent, or `None` while there is room left.
    pub fn exceeded_by(&self, totals: &UsageTotals) -> Option<String> {
        if self.max_total_tokens > 0 && totals.total_tokens >= self.max_total_tokens {
            return Some(format!(
                "the token budget is spent: {} of {} tokens used across {} model pass(es)",
                totals.total_tokens, self.max_total_tokens, totals.passes
            ));
        }
        if self.max_total_cost > 0.0
            && let Some(cost) = totals.cost
            && cost >= self.max_total_cost
        {
            return Some(format!(
                "the cost budget is spent: {:.4} of {:.4} {} used across {} model pass(es)",
                cost,
                self.max_total_cost,
                totals.currency.as_deref().unwrap_or(""),
                totals.passes
            ));
        }
        None
    }
}

/// Token totals for one model, within a larger set of totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelTotals {
    /// Model name as the relay was asked for it.
    pub model: String,
    /// Passes billed to this model.
    pub passes: u32,
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Input tokens served from the prompt cache.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost under the configured price list, when there is one.
    pub cost: Option<f64>,
}

/// Everything spent so far, ready to display.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Per-request detail for new ledger records; old pass-only logs have no fabricated rows.
    #[serde(default)]
    pub calls: Vec<RequestCost>,
    /// Model-backed passes counted.
    pub passes: u32,
    /// Input tokens, cached ones included.
    pub input_tokens: u64,
    /// Input tokens served from the prompt cache.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input plus output.
    pub total_tokens: u64,
    /// Model requests made across those passes.
    pub requests: u32,
    /// Requests whose response reported no usage; their tokens are missing from these figures.
    pub requests_without_usage: u32,
    /// Cost under the configured price list; `None` when no price list is configured.
    pub cost: Option<f64>,
    /// Currency of `cost`, when there is one.
    pub currency: Option<String>,
    /// Per-model breakdown, largest spender first.
    pub by_model: Vec<ModelTotals>,
    /// The configured ceiling and how close the totals are to it.
    pub budget: Option<BudgetStatus>,
}

/// How the totals stand against the configured ceiling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetStatus {
    /// Token ceiling, zero when only a cost ceiling is set.
    pub max_total_tokens: u64,
    /// Cost ceiling, zero when only a token ceiling is set.
    pub max_total_cost: f64,
    /// Whether the ceiling has been reached; dispatch is frozen when it has.
    pub exceeded: bool,
    /// Why it is spent, when it is.
    pub reason: Option<String>,
    /// Fraction of the tightest configured ceiling that has been used, clamped to 1.0.
    pub used_fraction: f64,
}

impl UsageTotals {
    /// Totals over the `model.usage` records in an event log.
    ///
    /// The EventLog is the ledger: it is append-only, so a Job that was later superseded or
    /// replaced still has the tokens it really spent counted here.
    pub fn from_events(
        events: &[EventRecord],
        pricing: Option<&Pricing>,
        budget: &SpendBudget,
    ) -> Self {
        let ledger = request_ledger(events);
        let jobs: HashSet<_> = ledger.iter().filter_map(|r| r.job_id).collect();
        let namespaces: HashSet<_> = ledger.iter().map(|r| r.namespace).collect();
        let mut grouped: BTreeMap<Uuid, ModelUsage> = BTreeMap::new();
        for record in &ledger {
            if record.status == "not_sent" {
                continue;
            }
            let usage = record.counts();
            let sum = grouped
                .entry(record.namespace)
                .or_insert_with(|| ModelUsage {
                    model: usage.model.clone(),
                    ..Default::default()
                });
            sum.input_tokens = sum.input_tokens.saturating_add(usage.input_tokens);
            sum.cached_input_tokens = sum
                .cached_input_tokens
                .saturating_add(usage.cached_input_tokens);
            sum.output_tokens = sum.output_tokens.saturating_add(usage.output_tokens);
            sum.requests = sum.requests.saturating_add(usage.requests);
            sum.requests_without_usage = sum
                .requests_without_usage
                .saturating_add(usage.requests_without_usage);
        }
        let usages = events
            .iter()
            .filter(|event| event.kind == USAGE_EVENT_KIND)
            .filter(|event| event.job_id.is_none_or(|id| !jobs.contains(&id)))
            .filter(|event| {
                event.payload["snapshot_id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .is_none_or(|id| !namespaces.contains(&id))
            })
            .filter_map(|event| serde_json::from_value::<ModelUsage>(event.payload.clone()).ok())
            .chain(grouped.into_values());
        let mut totals = Self::from_usages(usages, pricing, budget);
        totals.calls = ledger
            .into_iter()
            .map(|request| {
                let counts = request.counts();
                let cost = if request.status == "not_sent"
                    || (!counts.is_complete() && counts.total_tokens() == 0)
                {
                    None
                } else {
                    pricing.map(|p| p.cost(&counts))
                };
                RequestCost {
                    request,
                    cost,
                    currency: pricing.map(|p| p.currency.clone()),
                }
            })
            .collect();
        totals
    }

    /// Totals over usage records from any source.
    pub fn from_usages(
        usages: impl IntoIterator<Item = ModelUsage>,
        pricing: Option<&Pricing>,
        budget: &SpendBudget,
    ) -> Self {
        let mut totals = Self {
            currency: pricing.map(|pricing| pricing.currency.clone()),
            cost: pricing.map(|_| 0.0),
            ..Self::default()
        };
        let mut by_model: Vec<ModelTotals> = Vec::new();
        for usage in usages {
            totals.passes += 1;
            totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
            totals.cached_input_tokens = totals
                .cached_input_tokens
                .saturating_add(usage.cached_input_tokens);
            totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
            totals.requests = totals.requests.saturating_add(usage.requests);
            totals.requests_without_usage = totals
                .requests_without_usage
                .saturating_add(usage.requests_without_usage);
            let cost = pricing.map(|pricing| pricing.cost(&usage));
            if let (Some(total), Some(cost)) = (totals.cost.as_mut(), cost) {
                *total += cost;
            }
            let entry = match by_model.iter_mut().find(|entry| entry.model == usage.model) {
                Some(entry) => entry,
                None => {
                    by_model.push(ModelTotals {
                        model: usage.model.clone(),
                        cost: pricing.map(|_| 0.0),
                        ..ModelTotals::default()
                    });
                    by_model.last_mut().expect("just pushed")
                }
            };
            entry.passes += 1;
            entry.input_tokens = entry.input_tokens.saturating_add(usage.input_tokens);
            entry.cached_input_tokens = entry
                .cached_input_tokens
                .saturating_add(usage.cached_input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(usage.output_tokens);
            if let (Some(total), Some(cost)) = (entry.cost.as_mut(), cost) {
                *total += cost;
            }
        }
        totals.total_tokens = totals.input_tokens.saturating_add(totals.output_tokens);
        by_model.sort_by(|a, b| {
            (b.input_tokens + b.output_tokens).cmp(&(a.input_tokens + a.output_tokens))
        });
        totals.by_model = by_model;
        if budget.is_set() {
            let reason = budget.exceeded_by(&totals);
            let by_tokens = (budget.max_total_tokens > 0)
                .then(|| totals.total_tokens as f64 / budget.max_total_tokens as f64);
            let by_cost = match (budget.max_total_cost > 0.0, totals.cost) {
                (true, Some(cost)) => Some(cost / budget.max_total_cost),
                _ => None,
            };
            totals.budget = Some(BudgetStatus {
                max_total_tokens: budget.max_total_tokens,
                max_total_cost: budget.max_total_cost,
                exceeded: reason.is_some(),
                reason,
                used_fraction: by_tokens
                    .into_iter()
                    .chain(by_cost)
                    .fold(0.0_f64, f64::max)
                    .clamp(0.0, 1.0),
            });
        }
        totals
    }

    /// One line for a terminal: tokens, cost, and any gap in the record.
    pub fn one_line(&self) -> String {
        let mut line = format!(
            "{} pass(es) · {} in ({} cached) + {} out = {} tokens",
            self.passes,
            self.input_tokens,
            self.cached_input_tokens,
            self.output_tokens,
            self.total_tokens
        );
        if let (Some(cost), Some(currency)) = (self.cost, self.currency.as_deref()) {
            line.push_str(&format!(" · {cost:.4} {currency}"));
        }
        if self.requests_without_usage > 0 {
            line.push_str(&format!(
                " · {} of {} request(s) reported no usage, so the real figure is higher",
                self.requests_without_usage, self.requests
            ));
        }
        if let Some(budget) = &self.budget {
            line.push_str(&format!(
                " · {:.0}% of budget",
                budget.used_fraction * 100.0
            ));
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(model: &str, input: u64, cached: u64, output: u64) -> ModelUsage {
        ModelUsage {
            model: model.into(),
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            requests: 2,
            requests_without_usage: 0,
        }
    }

    /// Cached tokens are billed once, at the cached rate when one is configured.
    #[test]
    fn cost_bills_cached_input_at_its_own_rate() {
        let pricing = Pricing {
            input_per_mtok: 3.0,
            cached_input_per_mtok: Some(0.3),
            output_per_mtok: 15.0,
            currency: "USD".into(),
        };
        // 1M input of which 500k cached, 200k output.
        let cost = pricing.cost(&usage("m", 1_000_000, 500_000, 200_000));
        assert!((cost - (1.5 + 0.15 + 3.0)).abs() < 1e-9, "got {cost}");

        // Without a cached rate the full input rate applies — overstating, never understating.
        let flat = Pricing {
            cached_input_per_mtok: None,
            ..pricing
        };
        let cost = flat.cost(&usage("m", 1_000_000, 500_000, 0));
        assert!((cost - 3.0).abs() < 1e-9, "got {cost}");
    }

    /// Totals split by model, and a missing price list leaves tokens counted but cost absent.
    #[test]
    fn totals_break_down_by_model_and_survive_a_missing_price_list() {
        let pricing = Pricing {
            input_per_mtok: 1.0,
            cached_input_per_mtok: None,
            output_per_mtok: 2.0,
            currency: "USD".into(),
        };
        let records = vec![
            usage("small", 1_000, 0, 100),
            usage("big", 500_000, 0, 100_000),
            usage("small", 1_000, 0, 100),
        ];
        let totals =
            UsageTotals::from_usages(records.clone(), Some(&pricing), &SpendBudget::default());
        assert_eq!(totals.passes, 3);
        assert_eq!(totals.total_tokens, 602_200);
        assert_eq!(totals.by_model[0].model, "big", "biggest spender first");
        assert_eq!(totals.by_model[1].passes, 2);
        // big: 0.5 in + 0.2 out; each small: 0.001 in + 0.000_2 out.
        assert!((totals.cost.unwrap() - (0.7 + 2.0 * 0.001_2)).abs() < 1e-9);
        assert!(totals.budget.is_none(), "no ceiling means no budget status");

        let unpriced = UsageTotals::from_usages(records, None, &SpendBudget::default());
        assert_eq!(unpriced.total_tokens, 602_200);
        assert_eq!(unpriced.cost, None);
        assert_eq!(unpriced.by_model[0].cost, None);
    }

    /// A ceiling reports how close it is, and says plainly when it is spent.
    #[test]
    fn budget_reports_headroom_and_then_refuses() {
        let budget = SpendBudget {
            max_total_tokens: 10_000,
            max_total_cost: 0.0,
        };
        let totals = UsageTotals::from_usages(vec![usage("m", 4_000, 0, 1_000)], None, &budget);
        let status = totals.budget.clone().unwrap();
        assert!(!status.exceeded);
        assert!((status.used_fraction - 0.5).abs() < 1e-9);

        let totals = UsageTotals::from_usages(vec![usage("m", 9_000, 0, 1_000)], None, &budget);
        let status = totals.budget.unwrap();
        assert!(
            status.exceeded,
            "at the ceiling it is spent, not nearly spent"
        );
        assert!(status.reason.unwrap().contains("10000 tokens"));
    }

    /// A relay that reports no usage is visible in every derived figure.
    #[test]
    fn requests_without_usage_are_reported_not_priced_as_zero() {
        let totals = UsageTotals::from_usages(
            vec![ModelUsage {
                model: "m".into(),
                requests: 3,
                requests_without_usage: 3,
                ..ModelUsage::default()
            }],
            None,
            &SpendBudget::default(),
        );
        assert_eq!(totals.total_tokens, 0);
        assert_eq!(totals.requests_without_usage, 3);
        assert!(
            totals.one_line().contains("reported no usage"),
            "the gap must be stated: {}",
            totals.one_line()
        );
    }
}
