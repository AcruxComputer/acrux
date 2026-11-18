CREATE TABLE "bridge_unpaid_withdrawals" (
    "program" VARCHAR(44) NOT NULL,
    "sequence" BIGINT NOT NULL,
    "cblock" BIGINT NOT NULL,
    "epoch" BIGINT NOT NULL,
    "reason" TEXT,
    "attempts" INTEGER NOT NULL DEFAULT 0,
    "next_attempt_at" TIMESTAMPTZ(3) NOT NULL,
    "proof" JSONB NOT NULL,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "bridge_unpaid_withdrawals_pkey" PRIMARY KEY ("program", "sequence")
);
CREATE INDEX "bridge_unpaid_withdrawals_program_next_attempt_at_idx" ON "bridge_unpaid_withdrawals"("program", "next_attempt_at");
