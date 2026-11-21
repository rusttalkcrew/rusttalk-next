UPDATE roles SET permissions = permissions | 256 WHERE id IN ('owner', 'admin');
