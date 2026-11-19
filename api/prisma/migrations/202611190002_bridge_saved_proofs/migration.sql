ALTER TABLE "bridge_relayer_states"
ADD COLUMN "saved_withdrawal_sequence" BIGINT NOT NULL DEFAULT 0;

CREATE TABLE "bridge_saved_cblock_proofs" (
    "program" VARCHAR(44) NOT NULL,
    "cblock" BIGINT NOT NULL,
    "proof" JSONB NOT NULL,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "bridge_saved_cblock_proofs_pkey" PRIMARY KEY ("program", "cblock")
);
