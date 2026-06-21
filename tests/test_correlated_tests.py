"""Tests for correlated test discovery and enhanced anti-gaming in run_improver.py."""
import os
import sys
import re
from pathlib import Path

import pytest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "improver"))

import run_improver  # noqa: E402


class TestAntiGamingReason:
    """Test the enhanced _anti_gaming_reason function."""

    def test_no_gaming(self):
        """No gaming when counts are stable or increased."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        tests = {"passed": 12, "failed": 0, "errors": 0, "skipped": 0, "collected": 12}
        result = run_improver._anti_gaming_reason(base_tests, tests, "")
        assert result is None

    def test_pass_count_fell(self):
        """Gaming detected when pass count fell."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        tests = {"passed": 8, "failed": 0, "errors": 0, "skipped": 2, "collected": 10}
        result = run_improver._anti_gaming_reason(base_tests, tests, "")
        assert result is not None
        assert "pass count fell" in result

    def test_collected_count_fell(self):
        """Gaming detected when collected count fell."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 8}
        result = run_improver._anti_gaming_reason(base_tests, tests, "")
        assert result is not None
        assert "collected count fell" in result

    def test_skip_markers_introduced(self):
        """Gaming detected when skip/xfail markers introduced."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        diff_text = "+@pytest.mark.skip\ndef test_something():\n    pass"
        result = run_improver._anti_gaming_reason(base_tests, tests, diff_text)
        assert result is not None
        assert "skip/xfail marker(s)" in result

    def test_error_count_increased(self):
        """Gaming detected when error count increased."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        tests = {"passed": 10, "failed": 0, "errors": 2, "skipped": 0, "collected": 10}
        result = run_improver._anti_gaming_reason(base_tests, tests, "")
        assert result is not None
        assert "error count increased" in result

    def test_skip_count_increased_significantly(self):
        """Gaming detected when skip count increased significantly."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 1, "collected": 11}
        tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 5, "collected": 15}
        result = run_improver._anti_gaming_reason(base_tests, tests, "")
        assert result is not None
        assert "skipped count increased significantly" in result

    def test_skip_count_increased_slightly(self):
        """No gaming for small skip count increases."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 1, "collected": 11}
        tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 2, "collected": 12}
        result = run_improver._anti_gaming_reason(base_tests, tests, "")
        assert result is None

    def test_no_base_tests(self):
        """No gaming check when no base tests provided."""
        tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        result = run_improver._anti_gaming_reason(None, tests, "")
        assert result is None

    def test_no_current_tests(self):
        """No gaming check when no current tests provided."""
        base_tests = {"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10}
        result = run_improver._anti_gaming_reason(base_tests, None, "")
        assert result is None


class TestNewSkipMarkers:
    """Test the _new_skip_markers function."""

    def test_no_skip_markers(self):
        """No skip markers in diff."""
        diff = "+def test_something():\n+    pass"
        result = run_improver._new_skip_markers(diff)
        assert result == []

    def test_pytest_skip_decorator(self):
        """Detect pytest skip decorator."""
        diff = "+@pytest.mark.skip\ndef test_something():\n    pass"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "pytest.mark.skip" in result[0]

    def test_pytest_skipif_decorator(self):
        """Detect pytest skipif decorator."""
        diff = "+@pytest.mark.skipif(True, reason='test')\ndef test_something():\n    pass"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "pytest.mark.skipif" in result[0]

    def test_pytest_xfail_decorator(self):
        """Detect pytest xfail decorator."""
        diff = "+@pytest.mark.xfail\ndef test_something():\n    pass"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "pytest.mark.xfail" in result[0]

    def test_pytest_skip_call(self):
        """Detect pytest.skip() call."""
        diff = "+def test_something():\n+    pytest.skip('reason')"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "pytest.skip(" in result[0]

    def test_pytest_xfail_call(self):
        """Detect pytest.xfail() call."""
        diff = "+def test_something():\n+    pytest.xfail('reason')"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "pytest.xfail(" in result[0]

    def test_unittest_skip(self):
        """Detect unittest.skip decorator."""
        diff = "+@unittest.skip('reason')\ndef test_something(self):\n    pass"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "unittest.skip" in result[0]

    def test_unittest_skip_test(self):
        """Detect unittest.skipTest call."""
        diff = "+def test_something(self):\n+    self.skipTest('reason')"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert ".skipTest(" in result[0]

    def test_raise_skiptest(self):
        """Detect raise SkipTest."""
        diff = "+def test_something():\n+    raise SkipTest('reason')"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "raise SkipTest" in result[0]

    def test_multiple_skip_markers(self):
        """Detect multiple skip markers."""
        diff = """+@pytest.mark.skip
def test_something():
    pass
+@unittest.skip('reason')
def test_other():
    pass"""
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 2

    def test_file_header_excluded(self):
        """File header line should be excluded."""
        diff = "+++ b/test_file.py\n+@pytest.mark.skip\ndef test_something():\n    pass"
        result = run_improver._new_skip_markers(diff)
        assert len(result) == 1
        assert "pytest.mark.skip" in result[0]

    def test_non_add_lines_excluded(self):
        """Non-add lines should be excluded."""
        diff = "-@pytest.mark.skip\ndef test_something():\n    pass"
        result = run_improver._new_skip_markers(diff)
        assert result == []


