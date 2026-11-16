-- One program id on both chains gives the same account addresses: unique per chain.
DROP INDEX "hubs_registry_address_key";
DROP INDEX "members_membership_address_key";
CREATE UNIQUE INDEX "hubs_chain_registry_address_key" ON "hubs"("chain", "registry_address");
CREATE UNIQUE INDEX "members_chain_membership_address_key" ON "members"("chain", "membership_address");
