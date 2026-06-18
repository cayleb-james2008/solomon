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


class TestFindCorrelatedTests:
    """Test the _find_correlated_tests function."""

    def test_empty_changed_files(self, tmp_path):
        """Empty changed files list should return empty list."""
        result = run_improver._find_correlated_tests([])
        assert result == []

    def test_no_modules_in_changed_files(self, tmp_path):
        """Changed files without .py extension should return empty list."""
        result = run_improver._find_correlated_tests(["README.md", "config.json"])
        assert result == []

    def test_private_modules_ignored(self, tmp_path):
        """Private modules (starting with _) should be ignored."""
        result = run_improver._find_correlated_tests(["_private.py", "__init__.py"])
        assert result == []

    def test_simple_module_extraction(self, tmp_path):
        """Test module extraction from simple file paths."""
        # Create a test directory structure
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        # Create a test file that imports config
        test_file = test_dir / "test_config.py"
        test_file.write_text("""
import config
from config import load_config
def test_something():
    pass
""")
        
        # Temporarily set REPO to our test directory
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._find_correlated_tests(["scripts/config.py"])
            assert len(result) == 1
            assert "tests/test_config.py" in result[0]
        finally:
            run_improver.REPO = original_repo

    def test_package_path_extraction(self, tmp_path):
        """Test module extraction from package-style file paths."""
        # Create a test directory structure
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        # Create a test file that imports from a package
        test_file = test_dir / "test_broker.py"
        test_file.write_text("""
from asmodeus.execution.broker import paper
import asmodeus.execution.broker.router
def test_broker():
    pass
""")
        
        # Temporarily set REPO to our test directory
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._find_correlated_tests(["asmodeus/execution/broker.py"])
            assert len(result) == 1
            assert "tests/test_broker.py" in result[0]
        finally:
            run_improver.REPO = original_repo

    def test_multiple_correlated_tests(self, tmp_path):
        """Test that multiple test files can be correlated with the same change."""
        # Create a test directory structure
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        # Create multiple test files that import config
        for i in range(3):
            test_file = test_dir / f"test_config_{i}.py"
            test_file.write_text(f"""
import config
def test_something_{i}():
    pass
""")
        
        # Temporarily set REPO to our test directory
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._find_correlated_tests(["scripts/config.py"])
            assert len(result) == 3
            for i in range(3):
                assert any(f"test_config_{i}.py" in r for r in result)
        finally:
            run_improver.REPO = original_repo

    def test_no_correlated_tests(self, tmp_path):
        """Test when no test files import the changed module."""
        # Create a test directory structure
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        # Create a test file that doesn't import config
        test_file = test_dir / "test_other.py"
        test_file.write_text("""
import os
def test_something():
    pass
""")
        
        # Temporarily set REPO to our test directory
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._find_correlated_tests(["scripts/config.py"])
            assert len(result) == 0
        finally:
            run_improver.REPO = original_repo


class TestExpandGateWithCorrelated:
    """Test the _expand_gate_with_correlated function."""

    def test_empty_changed_files(self):
        """Empty changed files should return original gate command."""
        result = run_improver._expand_gate_with_correlated("pytest", [])
        assert result == "pytest"

    def test_no_correlated_tests(self, tmp_path):
        """No correlated tests should return original gate command."""
        # Create a test directory with no relevant tests
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._expand_gate_with_correlated("pytest", ["scripts/config.py"])
            assert result == "pytest"
        finally:
            run_improver.REPO = original_repo

    def test_custom_gate_not_expanded(self, tmp_path):
        """Custom gate commands should not be expanded."""
        # Create a test directory structure
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        # Create a test file that imports config
        test_file = test_dir / "test_config.py"
        test_file.write_text("""
import config
def test_something():
    pass
""")
        
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._expand_gate_with_correlated("custom_gate_command", ["scripts/config.py"])
            assert result == "custom_gate_command"
        finally:
            run_improver.REPO = original_repo

    def test_default_pytest_expanded(self, tmp_path):
        """Default pytest gate should be expanded with correlated tests."""
        # Create a test directory structure
        test_dir = tmp_path / "tests"
        test_dir.mkdir()
        
        # Create a test file that imports config
        test_file = test_dir / "test_config.py"
        test_file.write_text("""
import config
def test_something():
    pass
""")
        
        original_repo = run_improver.REPO
        run_improver.REPO = tmp_path
        
        try:
            result = run_improver._expand_gate_with_correlated("", ["scripts/config.py"])
            assert "pytest" in result
            assert "test_config.py" in result
        finally:
            run_improver.REPO = original_repo


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


class TestGetChangedFilesForCorrelation:
    """Test the _get_changed_files_for_correlation function."""

    def test_no_changes(self, tmp_path):
        """No changes should return empty list."""
        # This test would need a git repo, so we'll skip it for now
        pytest.skip("Requires git repo setup")
