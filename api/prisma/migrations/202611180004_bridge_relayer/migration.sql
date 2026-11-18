CREATE TABLE "bridge_relayer_states" (
    "program" VARCHAR(44) NOT NULL,
    "wallet" VARCHAR(44) NOT NULL,
    "lamports" DECIMAL(20,0) NOT NULL,
    "last_cblock" BIGINT,
    "last_handed_epoch" BIGINT,
    "last_relayed_at" TIMESTAMPTZ(3),
    "next_withdrawal_sequence" BIGINT NOT NULL DEFAULT 0,
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "bridge_relayer_states_pkey" PRIMARY KEY ("program")
);
