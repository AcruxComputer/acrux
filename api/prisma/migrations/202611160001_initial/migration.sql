-- CreateSchema
CREATE SCHEMA IF NOT EXISTS "public";

-- CreateEnum
CREATE TYPE "HubStatus" AS ENUM ('pending', 'active', 'inactive');

-- CreateEnum
CREATE TYPE "LabRunStatus" AS ENUM ('queued', 'running', 'done', 'failed_to_start');

-- CreateTable
CREATE TABLE "hubs" (
    "code" VARCHAR(16) NOT NULL,
    "registry_address" VARCHAR(44) NOT NULL,
    "registrant" VARCHAR(44) NOT NULL,
    "ecef_x" INTEGER NOT NULL,
    "ecef_y" INTEGER NOT NULL,
    "ecef_z" INTEGER NOT NULL,
    "latitude" DOUBLE PRECISION NOT NULL,
    "longitude" DOUBLE PRECISION NOT NULL,
    "deposit_lamports" DECIMAL(20,0) NOT NULL,
    "status" "HubStatus" NOT NULL DEFAULT 'pending',
    "created_epoch" BIGINT NOT NULL,
    "last_active_epoch" BIGINT,
    "releasable_epoch" BIGINT,
    "indexed_slot" BIGINT NOT NULL,
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "hubs_pkey" PRIMARY KEY ("code")
);

-- CreateTable
CREATE TABLE "members" (
    "identity" VARCHAR(44) NOT NULL,
    "membership_address" VARCHAR(44) NOT NULL,
    "hub_code" VARCHAR(16) NOT NULL,
    "stake_lamports" DECIMAL(20,0) NOT NULL,
    "proposer_stake_lamports" DECIMAL(20,0) NOT NULL,
    "capped" BOOLEAN NOT NULL,
    "effective_epoch" BIGINT NOT NULL,
    "cooldown_until_epoch" BIGINT NOT NULL,
    "pending_hub_code" VARCHAR(16),
    "leaving_epoch" BIGINT,
    "indexed_slot" BIGINT NOT NULL,
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "members_pkey" PRIMARY KEY ("identity")
);

-- CreateTable
CREATE TABLE "registry_events" (
    "id" BIGSERIAL NOT NULL,
    "signature" VARCHAR(88) NOT NULL,
    "instruction_index" INTEGER NOT NULL,
    "slot" BIGINT NOT NULL,
    "kind" TEXT NOT NULL,
    "hub_code" VARCHAR(16),
    "identity" VARCHAR(44),
    "payload" JSONB NOT NULL,
    "occurred_at" TIMESTAMPTZ(3) NOT NULL,

    CONSTRAINT "registry_events_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "faucet_grants" (
    "id" TEXT NOT NULL,
    "address" VARCHAR(44) NOT NULL,
    "ip_hash" TEXT NOT NULL,
    "amount_lamports" DECIMAL(20,0) NOT NULL,
    "transaction_id" TEXT,
    "granted_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "faucet_grants_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "lab_runs" (
    "id" TEXT NOT NULL,
    "scenario" TEXT NOT NULL,
    "seed" TEXT NOT NULL,
    "parameters" JSONB NOT NULL,
    "status" "LabRunStatus" NOT NULL DEFAULT 'queued',
    "outcome" JSONB,
    "failure" JSONB,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "started_at" TIMESTAMPTZ(3),
    "finished_at" TIMESTAMPTZ(3),

    CONSTRAINT "lab_runs_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "lab_run_events" (
    "run_id" TEXT NOT NULL,
    "sequence" INTEGER NOT NULL,
    "type" TEXT NOT NULL,
    "relative_us" BIGINT NOT NULL,
    "event" JSONB NOT NULL,

    CONSTRAINT "lab_run_events_pkey" PRIMARY KEY ("run_id","sequence")
);

-- CreateTable
CREATE TABLE "facts_snapshots" (
    "id" TEXT NOT NULL,
    "source" TEXT NOT NULL,
    "sample_size" INTEGER NOT NULL,
    "measured_from" TIMESTAMPTZ(3) NOT NULL,
    "measured_to" TIMESTAMPTZ(3) NOT NULL,
    "facts" JSONB NOT NULL,

    CONSTRAINT "facts_snapshots_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "indexer_cursors" (
    "address" VARCHAR(44) NOT NULL,
    "latest_signature" VARCHAR(88),
    "latest_slot" BIGINT,
    "discovered_by" VARCHAR(88),
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "indexer_cursors_pkey" PRIMARY KEY ("address")
);

-- CreateIndex
CREATE UNIQUE INDEX "hubs_registry_address_key" ON "hubs"("registry_address");

-- CreateIndex
CREATE INDEX "hubs_status_idx" ON "hubs"("status");

-- CreateIndex
CREATE UNIQUE INDEX "members_membership_address_key" ON "members"("membership_address");

-- CreateIndex
CREATE INDEX "members_hub_code_effective_epoch_idx" ON "members"("hub_code", "effective_epoch");

-- CreateIndex
CREATE INDEX "registry_events_hub_code_slot_idx" ON "registry_events"("hub_code", "slot");

-- CreateIndex
CREATE UNIQUE INDEX "registry_events_signature_instruction_index_key" ON "registry_events"("signature", "instruction_index");

-- CreateIndex
CREATE UNIQUE INDEX "faucet_grants_transaction_id_key" ON "faucet_grants"("transaction_id");

-- CreateIndex
CREATE INDEX "faucet_grants_address_granted_at_idx" ON "faucet_grants"("address", "granted_at");

-- CreateIndex
CREATE INDEX "faucet_grants_ip_hash_granted_at_idx" ON "faucet_grants"("ip_hash", "granted_at");

-- CreateIndex
CREATE INDEX "lab_runs_created_at_idx" ON "lab_runs"("created_at");

-- CreateIndex
CREATE INDEX "facts_snapshots_measured_to_idx" ON "facts_snapshots"("measured_to");

-- AddForeignKey
ALTER TABLE "members" ADD CONSTRAINT "members_hub_code_fkey" FOREIGN KEY ("hub_code") REFERENCES "hubs"("code") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "registry_events" ADD CONSTRAINT "registry_events_hub_code_fkey" FOREIGN KEY ("hub_code") REFERENCES "hubs"("code") ON DELETE SET NULL ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "lab_run_events" ADD CONSTRAINT "lab_run_events_run_id_fkey" FOREIGN KEY ("run_id") REFERENCES "lab_runs"("id") ON DELETE CASCADE ON UPDATE CASCADE;
