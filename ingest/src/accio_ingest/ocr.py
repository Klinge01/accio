def ocr_image(png_bytes: bytes) -> str:
    from Foundation import NSData, NSDictionary
    from Vision import (
        VNImageRequestHandler,
        VNRecognizeTextRequest,
        VNRequestTextRecognitionLevelAccurate,
    )

    request = VNRecognizeTextRequest.alloc().init()
    request.setRecognitionLevel_(VNRequestTextRecognitionLevelAccurate)
    request.setRecognitionLanguages_(["de-DE", "en-US"])
    request.setUsesLanguageCorrection_(True)
    data = NSData.dataWithBytes_length_(png_bytes, len(png_bytes))
    handler = VNImageRequestHandler.alloc().initWithData_options_(data, NSDictionary.dictionary())
    success, error = handler.performRequests_error_([request], None)
    if not success:
        raise RuntimeError(f"Vision OCR failed: {error}")
    lines = []
    for observation in request.results() or []:
        candidates = observation.topCandidates_(1)
        if candidates:
            lines.append(str(candidates[0].string()))
    return "\n".join(lines)
