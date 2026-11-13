# Seams

kiln reaches every product through `kiln-seams` (`crates/kiln-seams`; the critical path's master
plan, §3, CP-M2): a trait for each, its simulator, and its conformance suite. A track that needs
another's component builds and tests against the simulator, and switches to the real one when it
lands; the real one passes the same suite.

Each section below says what a seam is, what implements it, what uses it, its methods, and its
contract, clause by clause. A clause's id is its conformance case's (`kiln_seams::<seam>::cases`),
and its words are the case's: `crates/kiln-seams/tests/doc_truth.rs` fails when this page and the
code part (a method or a clause added, removed or reworded on one side only).

How a suite runs: `kiln_seams::conformance::run(seam, &cases, make)` runs each case against a
fresh fixture from `make`, an implementation and what the case needs of it (a simulated clock to
move, a real one to wait out). `run_within(seam, &cases, make, deadline)` gives each case a
deadline too, a sleep on the implementation's runtime: a case it never answers fails, named, and
the suite goes on (kiln's runs give each 30 s). Every simulator is deterministic, in memory and
bounded: past a bound it refuses, typed, or lets its oldest go and says so (a read from before
them refused, typed, or a count of what went), never dropping silently.

## Clock

Time: a monotonic now (what durations and deadlines are measured by), the wall clock (what records
are stamped with), and sleeps.

- **Implementations:** `kiln::rt::RtClock` (kiln's runtime: `Instant`, the system's clock, the
  runtime's timers); `kiln_seams::clock::SimClock`, whose time moves only when a test moves it
  (`advance`, `advance_to_next`), its sleeps waking in deadline order, at most 1,048,576 of them
  waiting (one more completes at once, and `refusals()` counts it). A test waits for the code
  under test to come round to its next sleep (`sleeping_since`, `advance_and_settle`) without
  sleeping itself.
- **Used by:** the goal driver (`goals/mod.rs`, through the hub's clock) and the harness host
  (`harness/host.rs`: its watch every 10 s, the half second before its record is written).
- **Passed by:** both, every clause (`rt`'s tests run the suite on real time).

Methods: `now`, `unix_ms`, `sleep_until`, `sleep`, `unix_s`.

- **CLOCK-1** now() never goes back, and moves by the time that passes
- **CLOCK-2** unix_ms() is the wall clock, moving forward as time passes; unix_s() is it in seconds
- **CLOCK-3** a sleep until now or a deadline already past, or of zero, completes at once
- **CLOCK-4** a sleep never completes before its deadline
- **CLOCK-5** a sleep completes once its deadline has passed
- **CLOCK-6** sleeps complete in deadline order, whatever order they started in
- **CLOCK-7** a sleep of Duration::MAX waits as long as the platform's time can say, without overflowing
- **CLOCK-8** a thousand sleeps at once all complete once their deadlines pass

## Spawn

Tasks, run on their own.

- **Implementations:** `kiln::rt::RtSpawn` (kiln's runtime; a task it can't take is refused
  `Full`); `kiln_seams::spawn::SimSpawn`, which runs its tasks only in `run_until_stalled`, one
  poll each in the order they were woken, at most a million polls a run (a task that wakes itself
  forever ends the run at its budget), at most 10,000 tasks held.
- **Used by:** kiln-sim (CP-T6.1), to run kiln's loops on a seeded scheduler.
- **Passed by:** both, every clause.

Methods: `spawn_boxed`.

- **SPAWN-1** a spawned task runs to its end, and its handle gives its output
- **SPAWN-2** a task runs to its end whether or not its handle is kept
- **SPAWN-3** tasks run side by side: two that wait on each other both end
- **SPAWN-4** abort ends a task at its next wait: its handle says Aborted, and what came after never runs
- **SPAWN-5** a task that panics ends alone: its handle says Lost, and other tasks run on
- **SPAWN-6** a thousand tasks all run to their ends

## Records

Every record kiln keeps, by family and key, each at the revision of the commit that last wrote it.

- **Implementations:** Amphora's typed records (CP-T1.5, which takes this suite past 200 cases);
  until then two of kiln's own: a host's over kiln's embedded Turso (`kiln::records::turso`, a
  file per host, each commit synced before it's answered, commits that come together written
  together; CP-T3.2), the one `records::open` swaps for Amphora's; and the server's
  (`kiln::records::StoreRecords`, durable in kiln's store, until CP-T1.5 takes its place, copying
  each family through the seam);
  `kiln_seams::records::SimRecords`: linearizable, snapshot reads from each key's last 32
  versions and the last 100,000 deleted keys' histories (older: refused `Compacted`), a change
  feed of the last 100,000 changes (a read of changes or a wait from before it: refused
  `Compacted`), a million rows. Its horizon is the store's, not each key's: once any key has
  lost a version, a snapshot from before the oldest kept is refused for every key (coarse,
  never wrong). Its `compact` forgets the history before a revision at once, as a compaction
  does; a suite's fixture makes its store do that for RECORDS-14.
- **Used by:** sessions' inboxes (CP-T3.2, `harness/inbox`); deploying by API's records
  (docs/deploy-api.md §14.1); goal stacks, goal items, plans, plan runs and keep-going, once
  CP-T1.5 moves them off rusqlite; the session journal (CP-T3.1).
- **Passed by:** the simulator, every clause; kiln's Turso store, every clause
  (`records/turso.rs`'s test); kiln's store's, every clause; Amphora's, when CP-T1.5 lands it.
- **Bounds:** family names 1 to 64 of `a-z 0-9 _ - .`; keys 1 to 1024 bytes; values to 1 MiB;
  1000 conditions and 1000 writes a transaction; 1000 rows a page or a change read.

Methods: `get`, `scan`, `commit`, `commit_owned`, `changes`, `wait`, `head`.

`commit_owned` keeps the supplied allocation owner with the transaction and transient driver
bindings until the worker releases them, including when the caller cancels its wait. An
implementation without this guarantee refuses `Unavailable` before enqueueing the transaction.

- **RECORDS-1** a row written is read back with its value and its commit's revision; a key never written reads as none
- **RECORDS-2** every commit's revision is higher than every one before it, and head() is the newest
- **RECORDS-3** a write conditioned on a row's absence succeeds once; the next is refused Conflict and writes nothing
- **RECORDS-4** a write conditioned on a row's revision (compare-and-set) succeeds at the current one and is refused Conflict at a stale one, naming the current
- **RECORDS-5** a transaction is all or nothing: one failed condition and no write applies, the head unmoved; together, its writes share one revision
- **RECORDS-6** a deleted row reads as none, leaves scans, and satisfies an absence condition
- **RECORDS-7** a scan returns a family's rows in key order, within its prefix, after its cursor, at most its limit, saying when more follow; a limit of 0 is refused
- **RECORDS-8** families are apart: one key in two families is two rows
- **RECORDS-9** a family's change feed gives every change committed after a revision, in commit order, a delete as none, at most its limit
- **RECORDS-10** a wait completes once a change to the family is committed after its revision, at once when one already is
- **RECORDS-11** a snapshot read sees the store as it was just after its revision; one ahead of the head is refused Ahead
- **RECORDS-12** bad family names, empty or long keys, large values and large transactions are refused, typed, and write nothing
- **RECORDS-13** a transaction with no writes commits nothing and returns the head; a key written twice in one commit keeps the last
- **RECORDS-14** after a compaction, snapshots and the feed from its revision on read as before; from before it, a snapshot, a read of changes and a wait are each refused Compacted or answered right, at once

## Bus

Topics of messages, in order, read from any offset kept, each topic of a durability class
(Focal's doc 33: ephemeral, replicated, persisted, windowed).

- **Implementations:** Quiver (CP-T2.1, `kiln::bus`: the vendored `quiver` node, on Focal's bus,
  in the server's process; it serves the ephemeral class, and opened as a node of a cluster the
  persisted class (CP-T2.2: each topic a part of Focal's replicated by the cluster's nodes, a
  group of one on one server, its payloads 1,102 bytes under the limit), and refuses the others,
  typed, until their goals land); `kiln_seams::bus::SimBus`: in memory (ephemeral, whatever class
  is asked), messages stamped by its clock, 10,000 topics.
- **Used by:** the UI's live feed of session events (CP-T2.1, docs/bus.md), then CI and CD
  events, webhooks and sends.
- **Passed by:** the simulator and Quiver, every clause; Quiver for its ephemeral and persisted
  classes alike (a fixture names the class its cases' topics take).
- **Bounds:** topic names 1 to 128 of `A-Z a-z 0-9 _ - . /`; payloads to 1 MiB, keys to 1024
  bytes; 1000 messages a read; a topic retains 1 to a million messages.

Methods: `create`, `publish`, `read`, `wait`, `bounds`, `delete`.

- **BUS-1** a topic's messages get offsets 0, 1, 2… in the order they were published
- **BUS-2** a read from an offset gives the messages from it in publish order, keys and payloads intact, and where to go on
- **BUS-3** every reader reads every message kept: readers don't take them from each other
- **BUS-4** a topic keeps its newest messages up to its retention; a read from before them is refused Behind, naming where they start
- **BUS-5** a wait completes once a message at its offset or later is there, at once when one is
- **BUS-6** a publish to or read of a topic that doesn't exist is refused NoTopic
- **BUS-7** creating a topic again with the same settings changes nothing; with others it's refused Exists; bad names and a retention of 0 are refused
- **BUS-8** a payload over the limit is refused TooLarge and publishes nothing
- **BUS-9** a read gives at most its limit and says where to go on; at the end it's empty; a limit of 0 is refused
- **BUS-10** topics are apart: each has its own messages and offsets
- **BUS-11** a deleted topic is gone: its waits, publishes, reads and deletes are refused NoTopic, and created again its offsets start at 0

## Blobs

Bytes, with S3's semantics. An object's etag is its content's SHA-256, however it was uploaded. A
listing names each object's key, size and modified time, not its etag (an S3's listing carries its
own ETags, MD5s or digests of parts, not the content's SHA-256): head says that. Head, get and list
say the same modified time, the store's, to its resolution (an S3's is a whole second); a put's
answer says when the store acknowledged it, as near as the store tells. What every implementation
takes is checked by the same functions (`check_key`, `check_sha256`, `byte_range`, `check_parts`,
`lifetime_secs` in `kiln_seams::blobs`), so a key, range or lifetime one store takes, every store
takes. A put is checked in this order: its key, its size, its checksum, then its condition.

- **Implementations:** kiln's local store (`kiln::blobs::local::LocalBlobs`: kiln's
  content-addressed store, its data where it was), any S3 (`kiln::blobs::s3::S3Blobs`), and Mantle
  over its native protocol (`kiln::blobs::mantle::MantleBlobs`, CP-T10.2);
  `kiln_seams::blobs::SimBlobs`: in memory, its presigned URLs signed and timed by its clock,
  objects to 64 MiB, a million of them, a GiB in all (uploads' parts included), 1000 uploads under
  way.
- **Used by:** kiln's content-addressed store (caches, artifacts, registry blobs, release assets and
  test reports, which CD reads its artifacts from), sync's blobs, recordings, transcripts and
  runner upgrade binaries (docs/blobs.md); Reliquary's bytes (CP-T9.1).
- **Passed by:** the simulator, every clause; the local store, every clause; the S3 store against
  MinIO, every clause (docs/blobs.md, "The suite against MinIO").
- **Bounds:** keys 1 to 960 bytes (S3's 1024, less 64 a store may put before them), in segments of
  1 to 254 bytes between `/`s (a file name's 255, less the space an S3 store ends a key with), none
  `.` or `..` and none ending in a space, with no control characters and no backslash;
  1000 keys a listing (S3's); part numbers 1 to 10,000, every part but the last at least 5 MiB
  (S3's); presigned URLs for up to an hour (S3 allows 7 days: a URL is a credential, kept short),
  their lifetimes rounded up to whole seconds. A presigned URL's signature covers its method, its
  exact key, its expiry and, for a put, the length of the body it takes.

Methods: `put`, `get`, `head`, `delete`, `list`, `start_upload`, `upload_part`, `complete_upload`, `abort_upload`, `presign`.

- **BLOBS-1** an object put is got back whole, with its size and its content's SHA-256 as its etag
- **BLOBS-2** a get with a byte range gives those bytes (an end past the object, to its end); a range outside it is refused InvalidRange
- **BLOBS-3** head gives an object's metadata; head or get of a key with no object is refused NotFound
- **BLOBS-4** a put over an object replaces it, and new content has a new etag
- **BLOBS-5** a put If-None-Match creates only where there's no object; else it's refused PreconditionFailed and stores nothing
- **BLOBS-6** a put If-Match replaces only the object with that etag; else it's refused PreconditionFailed
- **BLOBS-7** a deleted object is gone; deleting a key with no object succeeds
- **BLOBS-8** a listing gives a prefix's objects in key order, after its cursor, at most its limit, saying when it's truncated; a limit of 0 is refused
- **BLOBS-9** a put with a SHA-256 its content doesn't have is refused BadChecksum and stores nothing
- **BLOBS-10** a multipart upload's parts, uploaded in any order, are joined in part order into one object
- **BLOBS-11** a multipart upload is refused when a part but the last is under the minimum or parts are named out of order; part numbers run 1 to 10,000
- **BLOBS-12** an aborted upload is gone: completing it is refused NoSuchUpload
- **BLOBS-13** a presigned get works until it expires by the store's clock, then is refused Expired; a URL's lifetime is more than 0 and at most an hour
- **BLOBS-14** a presigned put stores what its holder sends; a URL changed after signing is refused
- **BLOBS-15** keys are 1 to 960 bytes: an empty or longer one is refused Invalid
- **BLOBS-16** a key of multibyte UTF-8 is kept as it is: its object is got back by it, and head names it so
- **BLOBS-17** keys with spaces and URL-reserved characters are kept exactly, each its own object, and listed as they were put
- **BLOBS-18** a space, a plus, a slash and an escaped slash make different keys: "a b", "a+b", "a/b" and "a%2Fb" are four objects
- **BLOBS-19** keys that differ only in case are different objects
- **BLOBS-20** keys that differ only in Unicode normalization (NFC and NFD) are different objects: keys are their bytes
- **BLOBS-21** a key with a control character (NUL, tab, newline, DEL, a C1 control) is refused Invalid
- **BLOBS-22** a key with a backslash is refused Invalid, so keys are the same on every platform's file system
- **BLOBS-23** a key that starts or ends with '/', or holds an empty segment ('//'), is refused Invalid
- **BLOBS-24** a key with a '.' or '..' segment is refused Invalid; dots within a segment's name ('a.b', '..c', '...') are taken
- **BLOBS-25** a key's segments are 1 to 254 bytes: one of 254 is taken, last or not; one of 255 refused Invalid
- **BLOBS-26** a key's bytes are counted, not its characters: 960 bytes, most of them two-byte characters, are taken, one byte more refused Invalid
- **BLOBS-27** every request that names a key refuses a bad one Invalid: get, head, delete, start_upload and presign as put does
- **BLOBS-28** a key refused stores nothing: nothing is listed after
- **BLOBS-29** keys are exact: " a" isn't "a", and "a " is no key at all (a segment doesn't end in a space)
- **BLOBS-30** objects are apart: a put to one key changes no other, however alike their keys ('a', 'a/b', 'ab', 'a b')
- **BLOBS-31** an empty object is kept: its size is 0, its etag the SHA-256 of nothing, and a get gives no bytes
- **BLOBS-32** every byte value round-trips: 0 to 255 and back down are got back exactly
- **BLOBS-33** objects of 1 byte, 64 KiB, 1 MiB and 1 MiB and a byte are each got back exactly
- **BLOBS-34** an object of 9 MiB of noise is got back exactly, its etag its SHA-256
- **BLOBS-35** an object as large as the store takes is kept; one byte more is refused TooLarge and stores nothing
- **BLOBS-36** a put's answer says the key, size and etag head then says
- **BLOBS-37** an etag is 64 lowercase hex digits
- **BLOBS-38** a get's answer carries the object's key, size and etag, as head says them
- **BLOBS-39** the same content put again keeps its etag, and the object is got back unchanged
- **BLOBS-40** a smaller object put over a larger leaves exactly the new bytes, no tail of the old
- **BLOBS-41** an empty object put over another leaves it empty
- **BLOBS-42** two keys may hold the same content, each its own object: deleting one leaves the other whole
- **BLOBS-43** replacing one of two keys that held the same content leaves the other's as it was
- **BLOBS-44** content a bit apart has etags apart
- **BLOBS-45** the same content under two keys has one etag
- **BLOBS-46** a put with its content's SHA-256 is taken, in lower or upper case hex
- **BLOBS-47** a put whose SHA-256 isn't 64 hex digits is refused Invalid and stores nothing
- **BLOBS-48** a put with a wrong SHA-256 over an object leaves the object as it was
- **BLOBS-49** an empty object put with the SHA-256 of nothing is taken
- **BLOBS-50** a put's checksum is checked before its condition: a wrong one is refused BadChecksum even where If-None-Match fails too
- **BLOBS-51** a create (If-None-Match) with its content's SHA-256 is taken
- **BLOBS-52** a replace (If-Match) with its content's SHA-256 is taken
- **BLOBS-53** a create refused leaves the object as it was: its bytes, its etag and its time
- **BLOBS-54** a key whose object was deleted can be created again with If-None-Match
- **BLOBS-55** If-Match an object's etag holds after the same content is put again
- **BLOBS-56** If-Match an etag the object hasn't (empty, garbage, another's) is refused PreconditionFailed, and the object stays
- **BLOBS-57** If-Match the etag of an object since deleted is refused PreconditionFailed, and nothing is stored
- **BLOBS-58** of 8 creates (If-None-Match) of one key at once, exactly one is taken, the rest refused PreconditionFailed, and the object is the one taken
- **BLOBS-59** of 8 replaces at once, each If-Match the same etag, exactly one is taken, the rest refused PreconditionFailed, and the object is the one taken
- **BLOBS-60** of 8 unconditional puts of one key at once, every one is taken and the object is one of them whole
- **BLOBS-61** a create where an upload was aborted is taken: the upload left nothing
- **BLOBS-62** after a delete, If-Match any etag is refused PreconditionFailed
- **BLOBS-63** bytes 0-0 give the first byte; the last byte's range, closed or open, gives the last
- **BLOBS-64** a range from 0 to the last byte, closed or open, gives the whole object
- **BLOBS-65** a range's answer says the whole object's key, size and etag, not the range's
- **BLOBS-66** any range of an empty object is refused InvalidRange, naming size 0
- **BLOBS-67** a range that starts at or past the end is refused InvalidRange, naming the object's size; one on the last byte gives it
- **BLOBS-68** a range of a key with no object is refused NotFound, not InvalidRange
- **BLOBS-69** 40 ranges spread over 3 MiB of noise each give exactly their bytes
- **BLOBS-70** an object read in 1 MiB ranges, put together, is the object
- **BLOBS-71** a range across the joins of a multipart object's parts gives its bytes
- **BLOBS-72** ranges of one byte and of three in an object's middle give exactly those
- **BLOBS-73** head after a delete is refused NotFound
- **BLOBS-74** an object being uploaded in parts isn't there until its upload completes: head and get are refused NotFound
- **BLOBS-75** head, get and list say the same size and modified time of an object
- **BLOBS-76** an object written again later has a later modified time, by the store's clock
- **BLOBS-77** a key between two objects' ('a' and 'a/b/c' put, 'a/b' not) is refused NotFound: keys aren't folders
- **BLOBS-78** deleting a key leaves keys that start with it ('a/b', 'ab') as they were
- **BLOBS-79** a key put again after a delete has the new content, size and etag
- **BLOBS-80** deleting a key with no object succeeds, again and again
- **BLOBS-81** a deleted object leaves the listing at once
- **BLOBS-82** content deleted under one key and put again stays whole under another that holds it too
- **BLOBS-83** an empty store's listing is empty and not truncated
- **BLOBS-84** a listing of the prefix "" gives every object
- **BLOBS-85** a prefix is a string, not a folder: "p" lists "p", "p/x", "pa" and "pb", not "q"
- **BLOBS-86** keys list in the order of their UTF-8 bytes: A, Z, a, a/b, a0, aa, z, é
- **BLOBS-87** a prefix that names nothing lists nothing, and isn't truncated
- **BLOBS-88** a listing's limit of 1 gives one object, truncated when more follow
- **BLOBS-89** a listing whose limit is what's there, or more, gives it all and isn't truncated
- **BLOBS-90** a limit over 1000 gives 1000 and says the rest follow; paged, 1001 objects are each listed
- **BLOBS-91** a listing after a key starts past it, whether or not the key is there
- **BLOBS-92** a listing after a key before its prefix starts at the prefix
- **BLOBS-93** a listing after the last key, or past it, is empty and not truncated
- **BLOBS-94** paging with a limit of 2, each page after the one before's last key, visits every object once, in order
- **BLOBS-95** a listing says each object's size and modified time as head says them
- **BLOBS-96** a listing after an overwrite says the new size
- **BLOBS-97** an upload under way isn't listed; once completed, its object is
- **BLOBS-98** an empty object is listed, with size 0
- **BLOBS-99** multibyte and reserved keys are listed as they were put
- **BLOBS-100** objects stored by a presigned put and by an upload are listed like any other
- **BLOBS-101** a put refused for its condition, checksum or size lists nothing new
- **BLOBS-102** a prefix that is a whole key lists that object and those that go on from it
- **BLOBS-103** a prefix with reserved characters ("s/q?") matches them exactly
- **BLOBS-104** a cursor within a prefix lists only the prefix's later keys
- **BLOBS-105** an object put after a page was read is on a later page when its key is past the cursor
- **BLOBS-106** a key of 960 bytes is listed and deleted like any other
- **BLOBS-107** parts uploaded in order and completed are one object, its etag the SHA-256 of the whole
- **BLOBS-108** an upload of one part smaller than the minimum is one object: the last part may be any size
- **BLOBS-109** an object put in parts has the etag of the same bytes put whole
- **BLOBS-110** a part uploaded again replaces the one before: completing with the old etag is refused BadParts, with the new it's taken
- **BLOBS-111** completing with a part named twice is refused BadParts
- **BLOBS-112** completing with no parts named is refused BadParts
- **BLOBS-113** completing with a part never uploaded is refused BadParts
- **BLOBS-114** completing with a part's etag wrong is refused BadParts
- **BLOBS-115** parts 1 and 10,000 are taken; 0 and 10,001 are refused Invalid
- **BLOBS-116** completing or aborting an upload that was never started is refused NoSuchUpload
- **BLOBS-117** a completed upload is gone: completing it again, aborting it or a part for it is refused NoSuchUpload
- **BLOBS-118** a part for an aborted upload is refused NoSuchUpload
- **BLOBS-119** a completed upload replaces the object at its key
- **BLOBS-120** aborting an upload leaves the object at its key as it was
- **BLOBS-121** two uploads to one key are apart, each with its own parts; the one completed last is the object
- **BLOBS-122** each upload started has an id of its own
- **BLOBS-123** a part over the store's object limit is refused TooLarge
- **BLOBS-124** completing an upload whose parts sum past the object limit is refused TooLarge and stores nothing
- **BLOBS-125** parts of exactly the minimum, then a last of one byte, are taken
- **BLOBS-126** parts of one upload uploaded at once are each kept and joined in order
- **BLOBS-127** deleting a key doesn't end an upload to it: the upload completes after, and is the object
- **BLOBS-128** a put while an upload is under way is replaced when the upload completes
- **BLOBS-129** a part uploaded again and again is joined as its last upload
- **BLOBS-130** an upload aborted after parts lists nothing
- **BLOBS-131** a completed upload's object is headed and listed like any other, with one modified time
- **BLOBS-132** parts of different content have different etags
- **BLOBS-133** a presigned get of a key with no object is refused NotFound
- **BLOBS-134** a presigned get gives the object as it is when used: put again since signing, the new bytes
- **BLOBS-135** a presigned put replaces the object at its key
- **BLOBS-136** an object stored by a presigned put has its content's SHA-256 as its etag
- **BLOBS-137** a presigned put that has expired is refused Expired and stores nothing
- **BLOBS-138** a get's URL can't put and a put's can't get: each is refused
- **BLOBS-139** a URL changed to name another key is refused
- **BLOBS-140** presigning makes nothing: a presigned put's key has no object until its URL is used
- **BLOBS-141** a presigned get of an object deleted since signing is refused NotFound
- **BLOBS-142** a presigned put of no bytes stores an empty object
- **BLOBS-143** a presigned URL works for a multibyte key
- **BLOBS-144** a presigned URL works for a key with spaces and reserved characters
- **BLOBS-145** a presigned get gives an object of 2 MiB whole
- **BLOBS-146** puts to 16 keys at once all land, each got back as put
- **BLOBS-147** gets during a replacement each give one version whole, the old or the new
- **BLOBS-148** deletes of 10 keys at once leave none listed
- **BLOBS-149** of If-Match and If-None-Match puts racing on one object, exactly one If-Match is taken
- **BLOBS-150** 12 uploads started at once to 12 keys each complete as their own object
- **BLOBS-151** a put's answer and head name the object's key
- **BLOBS-152** a multipart object got whole says its whole size and etag
- **BLOBS-153** ranges of an object stored by a presigned put give its bytes
- **BLOBS-154** after a large object is replaced by a small one, a range past the small one's end is refused InvalidRange naming its size
- **BLOBS-155** an upload completes with the parts it names, in their order, gaps allowed; small parts not last are refused BadParts
- **BLOBS-156** parts 1 and 9, with none between, complete into one object of the two
- **BLOBS-157** parts uploaded but not named are left out of the object, and gone with the upload
- **BLOBS-158** If-Match chains: each replace names the etag the last answered, and an older etag is refused PreconditionFailed
- **BLOBS-159** If-Match compares content, not size: an etag of same-sized, other content is refused PreconditionFailed
- **BLOBS-160** objects stored by a put, an upload and a presigned put each have their content's SHA-256 as their etag
- **BLOBS-161** a presigned put takes a body of exactly the length its URL declares: one longer or shorter is refused and stores nothing
- **BLOBS-162** a listing by a prefix ending in a space gives only the keys that start with it, the space included
- **BLOBS-163** a key with a segment that ends in a space is refused Invalid; spaces elsewhere in a segment are taken, and listed as they were put

## Secrets

Secrets by path, after AWS Secrets Manager and Vault: versions, the stages `AWSCURRENT`,
`AWSPREVIOUS` and `AWSPENDING`, scheduled deletes, and leases. A value never shows: its `Debug`
and `Display` say only how long it is, and dropped, its buffer is zeroed (`zeroize`): a copy made
of it is the copy's to clear.

- **Implementations:** Veil (CP-T8.1); `kiln_seams::secrets::SimSecrets`: in memory, timed by its
  clock, 100,000 secrets and leases.
- **Used by:** kiln's deploy and CI secrets (behind the secrets service's `Backend`), once CP-T8.1
  lands Veil.
- **Passed by:** the simulator, every clause; Veil, when CP-T8.1 lands it.
- **Bounds:** names 1 to 512 of `A-Z a-z 0-9 / _ + = . @ -` and values to 64 KiB (Secrets
  Manager's); 100 names a listing; 100 versions kept a secret (the oldest without a stage go
  first); deletes 7 to 30 days out (Secrets Manager's); leases up to a day.

Methods: `create`, `put`, `get`, `promote`, `describe`, `list`, `delete`, `restore`, `lease`, `read_leased`, `renew`, `revoke`.

- **SECRETS-1** a secret created has one version, current, read back as created
- **SECRETS-2** a version put current becomes AWSCURRENT, and the one it replaced AWSPREVIOUS
- **SECRETS-3** a version put pending is read as AWSPENDING and isn't current until promoted; promoted, the old current is previous and the pending stage goes
- **SECRETS-4** a version is read by its id with the value it was made with, however many came after; an unknown one is refused NoSuchVersion
- **SECRETS-5** a value never shows: not in Debug or Display, a describe or a listing
- **SECRETS-6** creating a name that exists is refused Exists, reading one that doesn't NotFound; bad names and values over 64 KiB are refused
- **SECRETS-7** a delete is scheduled 7 to 30 days out: reads are refused Deleted until it's restored, and once the window passes it's gone
- **SECRETS-8** a lease reads the current value until its time passes by the store's clock, then is refused Expired; renewing extends it, revoking ends it
- **SECRETS-9** a listing gives names with a prefix in name order, after its cursor, at most its limit

## Artifacts

OCI repositories, as the Distribution Specification v1.1 has them: blobs by digest, manifests by
tag or digest, kept byte for byte, tags that move, and referrers.

- **Implementations:** Reliquary (CP-T9.1, from kiln's `ci/registry`, passing the OCI conformance
  suite); `kiln_seams::artifacts::SimArtifacts`: in memory, 100,000 repositories, and 100,000
  blobs, manifests and tags in each, a GiB in all.
- **Used by:** kiln's registry, with agents, harnesses, skills and MCP servers checked on push,
  once CP-T9.1 lands Reliquary.
- **Passed by:** the simulator, every clause; Reliquary, when CP-T9.1 lands it.
- **Bounds:** manifests to 4 MiB; names to 255 bytes; 1000 tags a listing.

Methods: `push_blob`, `blob`, `push_manifest`, `manifest`, `tags`, `delete_manifest`, `referrers`.

- **ARTIFACTS-1** a blob pushed is named by the SHA-256 of its bytes and read back by it; one never pushed is refused BLOB_UNKNOWN
- **ARTIFACTS-2** a blob pushed under a digest that isn't its own is refused DIGEST_INVALID and stored nowhere
- **ARTIFACTS-3** a manifest is read back by tag and by digest, byte for byte, with its media type; its digest is its bytes' SHA-256
- **ARTIFACTS-4** a manifest that references a blob or manifest not in the repository is refused MANIFEST_BLOB_UNKNOWN, and its tag isn't set
- **ARTIFACTS-5** a tag moves to the manifest last pushed under it; the one before stays, by its digest
- **ARTIFACTS-6** a repository's tags are listed in order, after a cursor, at most a limit
- **ARTIFACTS-7** a manifest deleted by digest is gone, and every tag on it with it; deleting it again is refused MANIFEST_UNKNOWN
- **ARTIFACTS-8** the referrers of a manifest are the manifests whose subject it is, filtered by artifact type when asked
- **ARTIFACTS-9** names, tags and digests outside the Distribution Specification's grammar are refused NAME_INVALID, TAG_INVALID and DIGEST_INVALID
- **ARTIFACTS-10** a repository sees only what was pushed to it
- **ARTIFACTS-11** a manifest pushed under another digest, not JSON, of another media type than it says, or over 4 MiB is refused

## Sandbox

A harness or a CI leg launched in a boundary (the host, a rootless OCI container, a microVM): its
program, its folder, its limits and the hosts it may reach, its stdio streamed back and its end said
once, last. Its limits are memory and processes, and its shares of the CPU and the disk, on cgroup
v2's weights (1 to 10,000, 100 an even share: a build held below an agent). The suite asks its
fixture for programs by what they do (echo, exit with a code, use memory, start processes,
connect), so each implementation supplies its own.

How a limit ends a run, as Linux's cgroup v2 has it (agreed with Hex's track, CP-T7.1): memory when
the boundary killed it for going over (`memory.events` counts an `oom_kill`); processes when it was
refused one past its limit (`pids.max` fails the fork, the run isn't stopped) and then failed. A run
refused a process that copes and ends well ends with its code.

- **Implementations:** `host` (today's launch), Shards, then Hex's classes: `process`, `oci`,
  `firecracker`, `shards` (CP-T4.1, CP-T7.1, CP-T7.2); `kiln_seams::sandbox::SimSandbox`: built-in
  programs that do those things on the host boundary (a container or a microVM is refused
  `Unsupported`), 1000 runs at once, 10,000 outputs unread a run (past it, the run is killed).
- **Used by:** every harness kiln launches and its CI legs, once CP-T4.1 and CP-T7.1 land them.
- **Passed by:** the simulator, every clause; `host`, Shards and Hex's, when their tracks land them.

Methods: `launch`, `boundaries`.

A launch gives a `Run`: stdin given to it in order, what it says (its output, then its end, once),
and a kill (its end, unless it had one, is `Killed`).

Methods of Run: `stdin`, `close_stdin`, `next`, `kill`.

- **SANDBOX-1** what a run writes reaches its stdout in order, then its end, then nothing
- **SANDBOX-2** stdin reaches the program in order, and closing it is the end of its input
- **SANDBOX-3** a run's stderr and exit code are said as they are, and its end comes once, last
- **SANDBOX-4** a run killed ends Killed, and takes no more input
- **SANDBOX-5** a run over its memory limit is ended OverLimit(Memory); under it, it runs to its end
- **SANDBOX-6** a run refused processes past its process limit, that then fails, ends OverLimit(Processes); under it, it runs to its end
- **SANDBOX-7** a run reaches only the hosts it's granted
- **SANDBOX-8** a launch of a program that doesn't exist is refused NoSuchProgram
- **SANDBOX-9** a run works in the folder it's given
- **SANDBOX-10** cpu and io weights outside cgroup v2's 1 to 10,000 are refused Invalid; at its ends a run goes ahead

## Placement

Where work runs, by the resources it needs and the labels it asks for. A placement never
over-commits a node; one that can't be made is refused with why, in words; releasing frees exactly
what was placed.

- **Implementations:** Hex (CP-T7.1, with priorities, quotas and fair share);
  `kiln_seams::placement::SimPlacement`: on the node with the most memory free, then by name, a
  million placements.
- **Used by:** builds that can't starve a machine, and microVM CI legs (CP-T7.1).
- **Passed by:** the simulator, every clause; kiln's headroom rule (`kiln_ci::sched::place`, the
  engine's and the v1 dispatcher's, docs/runners.md §5), every clause, its target all of a node;
  Hex, when CP-T7.1 lands it. Every case runs on the same two nodes
  (`kiln_seams::placement::conformance_nodes`).

Methods: `place`, `release`, `nodes`.

- **PLACEMENT-1** work placed is on a node whose use grows by what it needs
- **PLACEMENT-2** no node is ever over-committed: work that doesn't fit is refused Unplaceable, saying why
- **PLACEMENT-3** releasing frees exactly what was placed; releasing what isn't placed is fine
- **PLACEMENT-4** the same id placed again is where it is, and uses nothing more
- **PLACEMENT-5** work goes only on nodes with the labels it asks for; none with them, it's refused
- **PLACEMENT-6** a refusal's why names what's short
- **PLACEMENT-7** small work fills the cluster exactly to its capacity, and is placed again once some is released

## HarnessPort

kiln's agents as the code that drives them sees them: who they are, what their harness takes (its
command catalog), and commands to them. Generic over kiln's types: an agent is kiln's
`harness::session::Info`, a command its `harness::model::Command`.

- **Implementations:** kiln's harness registry (`harness/port.rs`: the hub's own sessions and every
  runner's); `kiln_seams::harness::SimHarness`, which keeps agents in memory and records each
  command it's handed, stamped by its clock: 100,000 agents, what the 100,000 closed last were
  sent, and 100,000 commands an agent (past it, refused `Full`). An older closed agent's
  deliveries go, and `forgotten()` counts them; it keeps nothing for an agent it doesn't know (a
  catalog, a machine away).
- **Used by:** the goal driver and keep-going (`goals/mod.rs`, `goals/keep.rs`); the inbox and the
  dispatcher of sends (CP-T3.2, CP-T3.3).
- **Passed by:** the simulator, every clause; kiln's registry, every clause (`harness/port.rs`'s
  test, each clause on a hub of its own). A message reaches an agent when its inbox takes it
  (CP-T3.2: once, whatever retried it); what reached one is what its inbox took, in order.

Methods: `agents`, `agent`, `catalog`, `send`, `send_status`.

`send_status` marks an explicitly replaceable status stream. Implementations that support durable
replacement retain every accepted original in history; other implementations deliver each original
normally. `Retained` means durable sender custody, while `Delivered` means the receiver has it.
An explicit `Cancelled` outcome preserves the original in history without claiming consumption.

- **HARNESS-1** an open agent is listed once by agents() and found by agent(id); one never opened is neither
- **HARNESS-2** a command to an agent the port doesn't know is refused NoSession
- **HARNESS-3** commands delivered to an agent reach it in the order they were sent, each once
- **HARNESS-4** a closed agent is no longer known, and a command to it is refused NoSession
- **HARNESS-5** what's sent to one agent never reaches another

## Forge

What a git host does with changes (docs/ci.md W26-R20), for Review in Ship: stacks, the host's
queue, updating a stale change, batched reviews, merging, what the host verified of a change's
commits' signatures (W27-R03), and what a repository has of each (`abilities`). Theirs where a
host has it, kiln's where it doesn't: a host lacking one refuses it `Unsupported`, naming what kiln
does instead, and one that verifies no signatures says each commit's is `Unknown`, and why.

| | GitHub | GitLab | Bitbucket |
|---|---|---|---|
| Stacks | the Stacks API; merged bottom-up only, in the background, with a linear history | stacked merge requests: one author, 20 at most; each merges into the one below, the next retargeted | none: kiln chains pull requests by their bases and holds their order with a commit status |
| Queue | its merge queue, where the repository has one (a change taken out takes those stacked above it) | merge trains (Premium), into the default branch | none: kiln's merge queue (W28) |
| Updating | update-branch against an expected head (the base merged in) | its rebase, in the background | none: kiln replays the change and pushes it with a lease |
| Any host | a commit kiln replayed, pushed with a lease on the expected head | | |
| Batched review | its pending review | its draft notes | held by kiln, then posted together |
| Merging | in the background, polled (`merge_result`), cancelled if the head moved | at once, against an expected head | at once, against an expected head |
| Signatures | each commit's `verification` on the pull request's commits, kept from the push on | each commit's signature, asked of it (none: unsigned) | none: each commit `Unknown` |

Reads are conditional: a read with the ETag of a change, or of the open changes, that hasn't
changed is `NotModified` and costs no request, as GitHub's 304s don't count. Every other request
counts against the host's rate limit, and past it requests are refused `RateLimited` with when it
resets, never dropped.

- **Implementations:** githost (`kiln::githost::forge`: kiln's GitHub App, GitLab and Bitbucket,
  over their REST APIs, every read conditional, the limit read from each host's rate-limit
  headers); `kiln_seams::forge::SimForge` (`github()`, `gitlab()`, `bitbucket()`): each host
  simulated in memory, deterministic, keeping its rules (GitLab's stacks detected from target
  branches, as GitLab detects them), with a commit graph per repository so
  staleness, linear history, rebases and leases are real. Its bounds: 10,000 changes and 100,000
  commits a repository, 1000 drafts a change, the last 10,000 background merges (an older one's
  id reads `NotFound`, as GitHub forgets a result after 24 hours), comments to 64 KiB, titles to
  256 bytes (past each, refused `Full` or `Invalid`); GitLab's 20 merge requests a stack at most;
  rate limits of GitHub 5000 requests an hour, GitLab 7200, Bitbucket 1000 (a GitHub App
  installation's, Self-Managed GitLab's default once on, Bitbucket Cloud's), on the simulator's
  own clock (`advance`). Bitbucket's drafts are kiln's to hold and cost the host nothing.
  `verifications` reads a change's first 250 commits (GitHub lists no more of a pull request's).
  A simulated commit is unsigned until `sign` signs it: by a key the host has as the signer's
  (verified on GitHub and GitLab, as each says it), or by one it hasn't (unverified).
- **Used by:** Review in Ship (docs/ci.md W26): keeping changes fresh (R16), landing through a
  host's own queue where a repository has one (R19), batched reviews (R07, R08), and the hosts'
  mirror of pull requests, which keeps what each host verified of each commit (W27-R03) in
  `verifications`' terms, read from the same shapes.
- **Passed by:** each simulator (GitHub's, GitLab's, Bitbucket's), every clause; githost against
  each simulator, served in its host's REST shape, every clause. The suite's fixture gives the
  host's git side too (pushes, the queue's checks, the clock), as the simulator the implementation
  reaches.

Methods: `abilities`, `open`, `change`, `changes`, `update`, `stack`, `stack_add`, `stack_of`, `unstack`, `merge`, `merge_result`, `enqueue`, `dequeue`, `queued`, `draft`, `drafts`, `publish`, `comments`, `rate`, `verifications`.

- **FORGE-1** abilities say what the repository has of stacks, a queue, updating, batched reviews and merging, as its host has them, its queue only while it's on
- **FORGE-2** a change opened is read back with its author, branches and head; one never opened is refused NotFound
- **FORGE-3** a read with the ETag of a change, or of the open changes, that hasn't changed is NotModified and costs no request; once it changes the read gives it, with a new ETag
- **FORGE-4** a stale change updated the host's own way at its expected head comes to have its base's head, at once or once the host is done; at another head it's refused Moved and nothing moves; a host with no way of its own refuses Unsupported
- **FORGE-5** a replayed commit is pushed to a change's branch only with a lease on its expected head: at another head it's refused Moved and the branch stays
- **FORGE-6** a host with stacks makes one of changes bottom to top, each based on the head of the one below, each saying its place; a change that doesn't chain, or is in a stack already, is refused Invalid
- **FORGE-7** a host that bounds its stacks takes a stack of its most, and refuses StackLimit a change past it or by another author than the stack's
- **FORGE-8** a host without stacks refuses them Unsupported, naming what kiln does instead
- **FORGE-9** a host that merges stacks bottom-up merges a change with every unmerged change below it, bottom first, onto the trunk in a linear history, and leaves those above it open on the trunk
- **FORGE-10** a host that retargets merges a stack's bottom change into the trunk and retargets the next onto the trunk
- **FORGE-11** a bottom-up stack whose history isn't linear isn't merged (refused NotLinear, or failing once polled) until it's updated
- **FORGE-12** a merge of a change whose head isn't the one expected is refused Moved, or fails once polled, and nothing merges; at its head it ends merged, at once or once polled
- **FORGE-13** a repository's queue on its host lands changes in the order they were queued, each at its expected head; where it has none, queueing is refused Unsupported, naming kiln's queue
- **FORGE-14** taking a change out of its host's queue takes out the changes stacked above it too, and leaves the rest queued
- **FORGE-15** a batched review's comments are seen only by their author until published, then all appear together, with its verdict
- **FORGE-16** every request but a read answered NotModified counts against the host's rate limit; past it requests are refused RateLimited, saying when it resets, nothing is done, and they go again once it has
- **FORGE-17** a merged change refuses updates, merges, queueing, stacking and drafts: Closed
- **FORGE-18** bad requests are refused Invalid and change nothing: a branch that isn't there, a change into itself or from a branch with one open, a blank title, a blank or long comment, an empty stack
- **FORGE-19** unstacking takes a stack's unmerged changes out of it, leaving those queued in place, and dissolves it once none are left; a host that can't refuses Unsupported
- **FORGE-20** a change's commits are read oldest first with what the host verified of each signature, and why: one by a key the host has as its signer's Verified, naming them; one by a key it hasn't Unverified; one unsigned Unsigned; a host that verifies none (its abilities say) says Unknown of each; a change never opened is refused NotFound

## Search

A tree's files, searched by their text or by the shape of their code, each hit with the definition
it sits in (docs/ci.md W26-R07, the search across a change). A tree is built once, under its id (a
git tree's SHA), searched as often as asked, and forgotten. Text is the query's words, as written
when they hold a capital and in any case when they don't, or the regex between its slashes; code is
an ast-grep pattern (`$A` one node, `$$$` any run of them) matched in each language the tree's
files are in, or one by name. A hit is the line a match starts on: its file, line, column, last
line and text, and the innermost definition holding it, by name and kind.

- **Implementations:** vorpal (`kiln_search::VorpalSearch`, vendored at `vendor/vorpal`: an index
  per tree in a folder of the server's cache, the files, each one's definitions by vorpal's outline
  and its trigrams in vorpal's trigram store, which prunes what a query reads; code by vorpal's
  pattern engine, on the tree-sitter grammars kiln takes, Markdown, which reads any text as prose,
  only when a query names it); `kiln_seams::search::SimSearch`, which
  keeps trees in memory and reads definitions and patterns line by line, holding 64 trees (past
  them, building another forgets the one searched least recently).
- **Used by:** a change's search (`crates/kiln/src/ci/search.rs`): a kiln diff's version, and a
  host's pull request at its mirrored head.
- **Passed by:** the simulator and vorpal, every clause (`crates/kiln-search`'s test).
- **Bounds:** tree ids 1 to 128 bytes; files to 1 MiB (a bigger one is left out, as a binary one
  is); 200,000 files and 512 MiB a tree; queries to 1000 bytes; 1000 hits a search, the total
  counting the rest; 500 characters of a hit's line.

Methods: `build`, `built`, `search`, `forget`.

- **SEARCH-1** a built tree is searched by its words: each hit is a line holding them, with its file, line, column and text, by path then line
- **SEARCH-2** each hit names the innermost definition it sits in, by name and kind, and a line in none names none
- **SEARCH-3** words with a capital match as written, words without one in any case, and a query between slashes is a regex
- **SEARCH-4** a code pattern finds each place of its shape, never a definition's own name, each with the definition it sits in
- **SEARCH-5** a query nothing answers finds nothing, and isn't refused
- **SEARCH-6** an empty or long query, a regex or a pattern that doesn't parse, or a language no one knows is refused BadQuery, saying why; a limit of 0 is refused
- **SEARCH-7** hits past the limit are left out, and the total counts them
- **SEARCH-8** a tree not built is refused NotBuilt; a forgotten tree reads as never built, and forgetting it again is fine
- **SEARCH-9** trees are apart: one path in two trees answers from each tree's own files
- **SEARCH-10** a binary file, one that isn't UTF-8, or one past the bound is left out and counted as skipped
- **SEARCH-11** building a tree already built keeps it as it was built
- **SEARCH-12** past its bound of trees, building another forgets the one searched least recently
- **SEARCH-13** a code pattern given a language, by name or alias, searches only that language's files

## Sigstore

An outside Fulcio and Rekor (docs/ci.md W18-R02, W27-R12). Fulcio certifies a key for whoever an
identity token names, the key's holder proving it has it by signing the token's subject; Rekor logs
entries, numbered in the order they come, promises each (its signed entry timestamp), proves each
is in its tree, and gives each back by its number.

- **Implementations:** kiln's clients of the Fulcio and Rekor `[ci.attest]` names: Rekor's v1 API
  (`kiln::ci::signing::Remote`: Fulcio's `/api/v2/signingCert`, Rekor's `/api/v1/log/entries`), or
  with `rekor_api = "v2"` Rekor v2's (`kiln::ci::signing::RemoteV2`, rekor-tiles: an entry logged at
  `/api/v2/log/entries`, hashedrekord the one kind it takes, read back from its tiles under
  `/api/v2` and proven against its checkpoint; it promises nothing, its checkpoints and proofs are
  what hold it);
  `kiln_seams::sigstore::SimSigstore`, which certifies with a CA of its own and logs in an RFC 6962
  tree of its own, its promises and checkpoints signed with a key of its own:
  `SimSigstore::trusted_root` is Sigstore's trust in it, so kiln's verifier verifies what it signs.
  Its keys come from a seed; its certificates are for code signing and valid ten minutes; its log
  holds 100,000 entries (past it, refused `Full`), each request a second after the last from the
  start it's given.
- **Used by:** kiln's keyless signing (`[ci.attest] signing = "keyless"`, `ci/signing.rs`), and the
  trail's anchors: each checkpoint the server signs, logged as a hashedrekord entry where
  `[ci.attest] rekor_url` names a Rekor (`trail.rs`, docs/ci.md W27-R09).
- **Passed by:** the simulator, every clause; kiln's clients, every clause, over Fulcio's and
  Rekor's HTTP APIs (v1, and v2's with its tiles) against an HTTP front of the simulator
  (`ci::signing`'s tests). Where a log takes hashedrekord entries alone (v2), the clauses log those,
  and SIGSTORE-7 asks no promise of a log that makes none (`SigstoreFixture::kinds`, `promises`). Not yet against
  a real Fulcio and Rekor (one deployed in kiln's cluster, W18-R02).

Methods: `certify`, `log`, `entry`.

- **SIGSTORE-1** a key with a token it takes is certified: a chain of PEM certificates, the key's first (for code signing, naming the token's issuer), then what issued it
- **SIGSTORE-2** a token that isn't one is refused Unauthorized; a key that isn't one, or a proof that isn't the key's, is refused Invalid, naming which
- **SIGSTORE-3** an entry logged is found again at its index, with the same uuid, body and time
- **SIGSTORE-4** entries are numbered in the order they're logged, and their times never go back
- **SIGSTORE-5** the same entry logged twice is refused Conflict, naming where it is
- **SIGSTORE-6** an index never logged is refused NotFound
- **SIGSTORE-7** an entry comes with the proof of its inclusion: its index in a tree of more entries than that, the hashes leading from it to the tree's root, and a checkpoint of that tree; and with the log's promise, where the log promises
- **SIGSTORE-8** an entry whose signature isn't its verifier's is refused Invalid, naming the entry
- **SIGSTORE-9** a hashedrekord entry (a SHA-256, and its key's signature over it) is logged and found again, keeping its hash; one whose signature is over another hash is refused Invalid, naming the entry

## DependencySources

Dependency source facts for locks (docs/ci.md W29-R23, R26): stable repository identity, a ref's
peeled commit, ancestry from a captured revision, a file at a selected commit, OCI manifest
bytes and the exact location and SHA-256 of a runtime archive. The adapter keeps credentials
private; none enters a request, answer or refusal. Resolving may refresh a private mirror;
it never writes to a host, announces a release or queues an update draft. Panes keep reading
their existing cached facts without fetching. A cached resolution says `fresh = false` and
cannot establish what an upstream ref names now. Missing or shallow history proves nothing.
Qualified refs are exact: a tag whose name looks like a qualified branch or tag cannot
substitute its history for that requested ref (W29-R23/R24).
A full commit SHA names that object, even if a tag has the same spelling.
Runtime requests can name an exact version; a newer default does not replace that version's
archive metadata (W29-R24). Code comparisons return complete changed file contents and modes
between captured commits of the same repository identity, including binary files, symlinks
and gitlinks. Diff drivers and text conversion cannot alter these facts. A comparison supplies
evidence for a review; it never grants approval or makes either commit executable (W29-R25).
The tags at a commit come from the same view as the revision they're asked of, in byte order:
a full-commit pin records the first of them as `gh actions-lock` does (W29-R23).

- **Implementations:** `kiln_seams::dependencies::SimDependencySources`, deterministic and in
  memory; kiln's git, host, OCI and runtime adapter (`ci::dependency_sources`).
- **Used by:** dependency lock preflight and explicit regeneration (W29-R23); acceptance policy
  remains pure code in kiln-ci.
- **Passed by:** the simulator's shared contract. The real adapter runs the same contract on
  isolated git repositories and HTTP fixtures in its tests.
- **Bounds:** names to 256 bytes; paths and URLs to 4096 bytes; files to 1 MiB; manifests to 4 MiB.
  A complete code difference has at most 4096 changed files and 4 MiB of paths and file bytes;
  overflow is refused, never truncated. Git diagnostics are drained separately and discarded.
  The simulator keeps 256 repositories, each with at most 4096 commits, 4096 files and 1024 refs,
  16 parents per commit, 64 MiB of repository file bytes across repositories; 256 manifests and
  runtime archives each, with at most 64 MiB of manifest bytes. Insertion past a table or byte
  bound is refused atomically as `Full`. A read past its byte bound is refused `TooLarge`.
  `tags_at` gives the first 1024 tags at a commit, in byte order.
  Runtime metadata is read with the existing downloader's bounds (at most 16 MiB); archive
  bytes are downloaded and checked later by the runner. No archive bytes cross this seam.
  The real adapter keeps an identity record beside its mirror, at most 8 KiB, bound to the
  actual source's host and repository. Changing a source mapping cannot borrow that identity.

Methods: `resolve`, `contains`, `file`, `manifest`, `runtime`, `compare`, `tags_at`.

- **DEPENDENCIES-1** a ref resolves to its source identity and peeled commit, retaining the request and qualified ref; HEAD names its default branch
- **DEPENDENCIES-2** moving a tag changes a new resolution, while a captured revision keeps its commit, history and file bytes
- **DEPENDENCIES-3** history means ancestry from the captured commit in that source; an unrelated object is not proof, and an unknown object is refused Missing
- **DEPENDENCIES-4** a file is read at the selected commit within captured history; outside history is refused NotReachable, a missing file Missing, and traversal Invalid
- **DEPENDENCIES-5** a file over the byte bound is refused TooLarge before its bytes are copied into an answer
- **DEPENDENCIES-6** a changed repository identity refuses the old revision as IdentityChanged, even when its name and commit remain
- **DEPENDENCIES-7** an unreachable upstream can answer from a prior mirror, explicitly marked not fresh, preserving its identity and bytes
- **DEPENDENCIES-8** an unreachable source without a prior mirror is refused Unavailable
- **DEPENDENCIES-9** access denial refuses cached resolutions and files as Denied
- **DEPENDENCIES-10** a manifest is bound to its bytes and requested digest, and runtime metadata names an immutable archive and SHA-256; credentials and unsupported runtimes are refused
- **DEPENDENCIES-11** repository URLs and git revision expressions are refused Invalid, and a missing ref is refused Missing
- **DEPENDENCIES-12** code comparisons retain both captured identities and commits, include complete changed file bytes and modes within their bounds, and obey source access without accepting a pin
- **DEPENDENCIES-13** the tags at a commit are its source's qualified tags pointing at it, in byte order; an untagged commit has none, a moved tag is found at its new commit, and an unknown commit is refused Missing
