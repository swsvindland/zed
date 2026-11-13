(STag
  "<" @open
  ">" @close)

(ETag
  "</" @open
  ">" @close)

(EmptyElemTag
  "<" @open
  "/>" @close)

("\"" @open
  "\"" @close)
