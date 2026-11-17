ALTER TABLE "buyback_cycles"
    ADD COLUMN "post_graduation_bps" INTEGER NOT NULL DEFAULT 2500,
    ADD COLUMN "claim_burn_amount" DECIMAL(20,0) NOT NULL DEFAULT 0,
    ADD CONSTRAINT "buyback_cycles_post_graduation_bps_check" CHECK ("post_graduation_bps" BETWEEN 0 AND 10000),
    ADD CONSTRAINT "buyback_cycles_claim_burn_amount_check" CHECK ("claim_burn_amount" >= 0);

CREATE TABLE "buyback_budgets" (
    "mint" VARCHAR(44) NOT NULL,
    "wallet" VARCHAR(44) NOT NULL,
    "owed_lamports" DECIMAL(20,0) NOT NULL DEFAULT 0,
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "buyback_budgets_pkey" PRIMARY KEY ("mint", "wallet"),
    CONSTRAINT "buyback_budgets_owed_lamports_check" CHECK ("owed_lamports" >= 0)
);
