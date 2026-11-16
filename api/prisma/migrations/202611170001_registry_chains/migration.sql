-- Hubs and members carry their chain: Acrux's own Hub Registry or Solana mainnet's.
ALTER TABLE "registry_events" DROP CONSTRAINT IF EXISTS "registry_events_hub_code_fkey";
ALTER TABLE "members" DROP CONSTRAINT IF EXISTS "members_hub_code_fkey";
ALTER TABLE "hubs" ADD COLUMN "chain" VARCHAR(8) NOT NULL DEFAULT 'solana';
ALTER TABLE "members" ADD COLUMN "chain" VARCHAR(8) NOT NULL DEFAULT 'solana';
ALTER TABLE "hubs" DROP CONSTRAINT "hubs_pkey", ADD CONSTRAINT "hubs_pkey" PRIMARY KEY ("chain", "code");
ALTER TABLE "members" DROP CONSTRAINT "members_pkey", ADD CONSTRAINT "members_pkey" PRIMARY KEY ("chain", "identity");
ALTER TABLE "members" ADD CONSTRAINT "members_chain_hub_code_fkey" FOREIGN KEY ("chain", "hub_code") REFERENCES "hubs"("chain", "code") ON DELETE RESTRICT ON UPDATE CASCADE;
