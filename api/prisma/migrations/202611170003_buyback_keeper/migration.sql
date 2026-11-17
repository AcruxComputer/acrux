CREATE TABLE "KeeperTransaction" (
    "key" TEXT NOT NULL,
    "state" TEXT NOT NULL,
    "signature" TEXT NOT NULL,
    "signedBase64" TEXT NOT NULL,
    "blockhash" TEXT NOT NULL,
    "lastValidBlockHeight" BIGINT NOT NULL,
    "contextSlot" BIGINT NOT NULL,
    "error" TEXT,
    "createdAt" TIMESTAMP(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updatedAt" TIMESTAMP(3) NOT NULL,
    CONSTRAINT "KeeperTransaction_pkey" PRIMARY KEY ("key")
);

CREATE UNIQUE INDEX "KeeperTransaction_signature_key" ON "KeeperTransaction"("signature");
CREATE INDEX "KeeperTransaction_state_createdAt_idx" ON "KeeperTransaction"("state", "createdAt");

CREATE TABLE "buyback_cycles" (
    "id" TEXT NOT NULL,
    "mint" VARCHAR(44) NOT NULL,
    "wallet" VARCHAR(44) NOT NULL,
    "state" TEXT NOT NULL,
    "swap_lamports" DECIMAL(20,0),
    "minimum_out" DECIMAL(20,0),
    "bought_amount" DECIMAL(20,0),
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" TIMESTAMPTZ(3) NOT NULL,
    CONSTRAINT "buyback_cycles_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "buyback_cycles_state_check" CHECK ("state" IN ('claim', 'swap', 'burn', 'done')),
    CONSTRAINT "buyback_cycles_amounts_check" CHECK ("swap_lamports" >= 0 AND "bought_amount" >= 0)
);

CREATE INDEX "buyback_cycles_mint_wallet_state_created_at_idx" ON "buyback_cycles"("mint", "wallet", "state", "created_at");

CREATE TABLE "buyback_rows" (
    "id" TEXT NOT NULL,
    "cycle_id" TEXT NOT NULL,
    "action" TEXT NOT NULL,
    "signature" VARCHAR(88) NOT NULL,
    "mint" VARCHAR(44) NOT NULL,
    "wallet" VARCHAR(44) NOT NULL,
    "lamports" DECIMAL(20,0) NOT NULL DEFAULT 0,
    "token_amount" DECIMAL(20,0) NOT NULL DEFAULT 0,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "buyback_rows_pkey" PRIMARY KEY ("id"),
    CONSTRAINT "buyback_rows_action_check" CHECK ("action" IN ('claim', 'swap', 'burn')),
    CONSTRAINT "buyback_rows_amounts_check" CHECK ("lamports" >= 0 AND "token_amount" >= 0),
    CONSTRAINT "buyback_rows_cycle_id_fkey" FOREIGN KEY ("cycle_id") REFERENCES "buyback_cycles"("id") ON DELETE RESTRICT ON UPDATE CASCADE
);

CREATE UNIQUE INDEX "buyback_rows_signature_key" ON "buyback_rows"("signature");
CREATE UNIQUE INDEX "buyback_rows_cycle_id_action_key" ON "buyback_rows"("cycle_id", "action");
CREATE INDEX "buyback_rows_mint_wallet_created_at_idx" ON "buyback_rows"("mint", "wallet", "created_at");
