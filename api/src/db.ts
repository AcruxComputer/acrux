import { PrismaClient } from '@prisma/client'
import { createDatabase } from './engines/server/db.js'

export function connectDatabase(url: string) {
  return createDatabase(url, adapter => new PrismaClient({ adapter }))
}
