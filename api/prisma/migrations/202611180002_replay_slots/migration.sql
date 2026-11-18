CREATE TABLE "replay_slots" (
    "slot" BIGINT NOT NULL,
    "transactions" INTEGER NOT NULL,
    "acrux_slots" INTEGER NOT NULL,
    "sandwiches" JSONB NOT NULL,
    "record" JSONB NOT NULL,
    "read_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT "replay_slots_pkey" PRIMARY KEY ("slot")
);
