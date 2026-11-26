//! Profile-derived payment intent. The type lives in `mesh-llm-payments-types`.
use crate::ledger::Ledger;
use anyhow::Result;
pub use mesh_llm_payments_types::intent::PaymentIntent;

impl Ledger {
    /// Derive request eligibility from the single spending policy. No separate opt-in.
    pub fn payment_intent(&self) -> Result<PaymentIntent> {
        let policy = self.policy()?;
        policy.validate()?;
        let (max_input_msat_per_million, max_output_msat_per_million) = policy.rate_ceiling();
        Ok(match policy.mode {
            crate::ledger::ApprovalMode::FreeOnly => PaymentIntent::FreeOnly,
            crate::ledger::ApprovalMode::Automatic => PaymentIntent::AllowPaid {
                max_input_msat_per_million,
                max_output_msat_per_million,
                max_total_msat: policy.daily_budget_msat.unwrap_or(0),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::Pricing;
    #[test]
    fn requests_cannot_expand_profile_authority() {
        let profile = PaymentIntent::AllowPaid {
            max_input_msat_per_million: 10,
            max_output_msat_per_million: 20,
            max_total_msat: 4000,
        };
        let broad = PaymentIntent::AllowPaid {
            max_input_msat_per_million: 100,
            max_output_msat_per_million: 200,
            max_total_msat: 8000,
        };
        let price = Pricing {
            input_msat_per_million: 10,
            output_msat_per_million: 20,
            cache_discount_percent: 0,
        };
        assert!(profile.restrict(&broad).permits(&price));
        let dearer = Pricing {
            output_msat_per_million: 21,
            ..price.clone()
        };
        assert!(!profile.restrict(&broad).permits(&dearer));
        assert!(!PaymentIntent::FreeOnly.restrict(&broad).permits(&price));
        assert!(!profile.restrict(&PaymentIntent::FreeOnly).permits(&price));
    }

    #[test]
    fn intent_defaults_free_and_survives_restart() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let price = Pricing {
            input_msat_per_million: 10,
            output_msat_per_million: 20,
            cache_discount_percent: 0,
        };
        {
            let ledger = Ledger::open(directory.path())?;
            assert!(!ledger.payment_intent()?.permits(&price));
            ledger.set_policy(&crate::ledger::Policy {
                mode: crate::ledger::ApprovalMode::Automatic,
                daily_budget_msat: Some(4000),
                ..Default::default()
            })?;
        }
        let ledger = Ledger::open(directory.path())?;
        let intent = ledger.payment_intent()?;
        // No per-request total cap: the daily budget gates admission elsewhere.
        assert!(intent.permits(&price));
        let expensive = Pricing {
            input_msat_per_million: 11,
            ..price
        };
        assert!(intent.permits(&expensive));
        ledger.set_policy(&crate::ledger::Policy::default())?;
        assert!(!ledger.payment_intent()?.permits(&expensive));
        assert!(
            serde_json::from_str::<PaymentIntent>(r#"{"mode":"allow_paid","max_total_msat":1000}"#)
                .is_err()
        );
        Ok(())
    }

    fn priced(input: u64, output: u64) -> Pricing {
        Pricing {
            input_msat_per_million: input,
            output_msat_per_million: output,
            cache_discount_percent: 0,
        }
    }

    fn automatic(input: Option<u64>, output: Option<u64>) -> crate::ledger::Policy {
        crate::ledger::Policy {
            mode: crate::ledger::ApprovalMode::Automatic,
            daily_budget_msat: Some(500_000),
            max_input_msat_per_million: input,
            max_output_msat_per_million: output,
        }
    }

    #[test]
    fn configured_ceiling_bounds_rates_under_at_and_over() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let ledger = Ledger::open(directory.path())?;
        ledger.set_policy(&automatic(Some(1_000), Some(3_000)))?;
        let intent = ledger.payment_intent()?;
        assert!(intent.permits(&priced(999, 2_999)));
        assert!(intent.permits(&priced(1_000, 3_000)));
        assert!(!intent.permits(&priced(1_001, 3_000)));
        assert!(!intent.permits(&priced(1_000, 3_001)));
        // The lab overpay: 50,000,000 msat per million each way.
        assert!(!intent.permits(&priced(50_000_000, 50_000_000)));
        Ok(())
    }

    #[test]
    fn unset_ceiling_uses_the_shipped_default() -> Result<()> {
        use mesh_llm_payments_types::control::DEFAULT_RATE_CEILING;
        let directory = tempfile::tempdir()?;
        let ledger = Ledger::open(directory.path())?;
        ledger.set_policy(&automatic(None, None))?;
        let intent = ledger.payment_intent()?;
        match DEFAULT_RATE_CEILING {
            Some((input, output)) => {
                assert!(intent.permits(&priced(input, output)));
                assert!(!intent.permits(&priced(input + 1, output)));
                assert!(!intent.permits(&priced(input, output + 1)));
            }
            None => assert!(intent.permits(&priced(u64::MAX, u64::MAX))),
        }
        // One side set: the other keeps the default.
        ledger.set_policy(&automatic(Some(10), None))?;
        let (_, default_output) = DEFAULT_RATE_CEILING.unwrap_or((u64::MAX, u64::MAX));
        let intent = ledger.payment_intent()?;
        assert!(intent.permits(&priced(10, default_output)));
        assert!(!intent.permits(&priced(11, 1)));
        Ok(())
    }

    #[test]
    fn ceiling_round_trips_and_older_policies_still_load() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let ledger = Ledger::open(directory.path())?;
        ledger.set_policy(&automatic(Some(7), Some(9)))?;
        let read = ledger.policy()?;
        assert_eq!(read.max_input_msat_per_million, Some(7));
        assert_eq!(read.max_output_msat_per_million, Some(9));
        let status = ledger.policy_status(1)?;
        assert_eq!(status["max_input_msat_per_million"], 7);
        assert_eq!(status["rate_ceiling_msat_per_million"]["output"], 9);
        // A policy stored before the ceiling existed has no ceiling fields.
        let old: crate::ledger::Policy =
            serde_json::from_str(r#"{"mode":"automatic","daily_budget_msat":500000}"#)?;
        assert_eq!(old.max_input_msat_per_million, None);
        assert!(serde_json::to_string(&old)?.find("max_").is_none());
        assert!(automatic(Some(0), None).validate().is_err());
        assert!(automatic(None, Some(0)).validate().is_err());
        assert!(ledger.set_policy(&automatic(Some(0), None)).is_err());
        Ok(())
    }
}
