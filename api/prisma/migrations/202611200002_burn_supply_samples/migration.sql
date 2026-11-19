CREATE TABLE "burn_supply_samples" (
    "program" VARCHAR(44) NOT NULL,
    "mint" VARCHAR(44) NOT NULL,
    "epoch" BIGINT NOT NULL,
    "amount" DECIMAL(20, 0) NOT NULL,
    "decimals" INTEGER NOT NULL,
    "sampled_at" TIMESTAMPTZ(3) NOT NULL,
    CONSTRAINT "burn_supply_samples_pkey" PRIMARY KEY ("program", "mint", "epoch")
);
