-- 0004: upload claims and failure cleanup (M1 review).
--
-- An upload claims its meeting before reading the body: status 'receiving' under a claim
-- id that expires at receiving_until (a crashed upload frees the meeting), with the bytes
-- it may store reserved for the workspace's storage plan. A transcode that fails for our
-- reasons past its attempts ends 'media_unavailable' (not 'transcoding' forever).
ALTER TABLE meetings DROP CONSTRAINT meetings_status_check;
ALTER TABLE meetings ADD CONSTRAINT meetings_status_check CHECK (status IN (
    'awaiting_consent', 'awaiting_media', 'receiving', 'transcoding', 'transcribing',
    'transcribed', 'transcription_unavailable', 'media_rejected', 'media_unavailable'));
ALTER TABLE meetings
    ADD COLUMN upload_claim uuid,
    ADD COLUMN receiving_until timestamptz,
    ADD COLUMN upload_reserved bigint CHECK (upload_reserved > 0),
    ADD CONSTRAINT meetings_claim_shape CHECK (
        (status = 'receiving') = (upload_claim IS NOT NULL AND receiving_until IS NOT NULL
                                  AND upload_reserved IS NOT NULL));
