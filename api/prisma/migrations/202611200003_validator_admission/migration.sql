ALTER TABLE "members"
    ADD COLUMN "validator_kind" VARCHAR(8),
    ADD COLUMN "admitted_at_epoch" BIGINT;
