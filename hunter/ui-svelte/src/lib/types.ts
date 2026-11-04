// API wire types, generated from the hunter-rs structs by ts-rs into
// ./generated/ (`just bindings` in hunter-rs/). Never declare a shape here:
// derive `TS` on the Rust type instead. This file only re-exports.
//
// Generated types say what the daemon sends, not what arrives: `api<T>()`
// casts without looking, so the runtime guard is still `validate.ts`.

export type { AdminTask } from "./generated/AdminTask";
export type { Event } from "./generated/Event";
export type { FindingDetail } from "./generated/FindingDetail";
export type { FindingOut } from "./generated/FindingOut";
export type { JobListEntry } from "./generated/JobListEntry";
export type { JobPhase } from "./generated/JobPhase";
export type { NextCandidate } from "./generated/NextCandidate";
export type { OpenPrOut } from "./generated/OpenPrOut";
export type { PrReviewIssueOut } from "./generated/PrReviewIssueOut";
export type { PrReviewOut } from "./generated/PrReviewOut";
export type { PrReviewRoundOut } from "./generated/PrReviewRoundOut";
export type { RepoBrief } from "./generated/RepoBrief";
export type { RepoNotesResponse } from "./generated/RepoNotesResponse";
export type { SessionUser } from "./generated/SessionUser";
export type { Stats } from "./generated/Stats";
export type { StatsHorizon } from "./generated/StatsHorizon";
export type { Summary } from "./generated/Summary";
